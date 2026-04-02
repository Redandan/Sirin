//! Background follow-up worker for Sirin.
//!
//! Runs every 30 minutes (or whenever the Tokio runtime is otherwise idle).
//! Each run:
//!
//! 1. Reads the last [`TASK_LOOKBACK`] lines from `data/tracking/task.jsonl`.
//! 2. Filters entries whose `status` is `"FOLLOWING"` or `"PENDING"`.
//! 3. Builds a prompt from the active [`Persona`] objectives + the filtered
//!    entries and sends it to a locally-running Ollama instance.
//! 4. If the model responds that a follow-up is needed, updates those entries'
//!    status to `"FOLLOWUP_NEEDED"` in the JSONL file.
//!
//! ## Ollama
//!
//! Expects Ollama at `http://localhost:11434`.  The model is taken from the
//! `OLLAMA_MODEL` environment variable and defaults to `"llama3"`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::persona::{Persona, TaskEntry, TaskTracker};

/// How many trailing log lines to inspect on each run.
const TASK_LOOKBACK: usize = 50;

/// Interval between worker runs (30 minutes).
const WORKER_INTERVAL_SECS: u64 = 30 * 60;

/// Default Ollama base URL.
const OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Default model name when `OLLAMA_MODEL` is not set.
const DEFAULT_MODEL: &str = "llama3.2";

// ── Ollama API types ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    prompt: String,
    /// Disable streaming so we get a single JSON response.
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    response: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build the prompt text sent to the local LLM.
fn build_prompt(persona: &Persona, entries: &[&TaskEntry]) -> String {
    let objectives = format!(
        "Persona: {} (v{})\nDescription: {}\nROI threshold: ${:.2} USD",
        persona.name, persona.version, persona.description, persona.roi_thresholds.min_trigger_usd
    );

    let tasks: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "- [{}] event={} status={} profit={:.2}",
                e.timestamp,
                e.event,
                e.status.as_deref().unwrap_or("?"),
                e.estimated_profit_usd.unwrap_or(0.0),
            )
        })
        .collect();

    format!(
        r#"You are an assistant reviewing pending tasks for an AI trading agent.

{objectives}

The following tasks are currently in PENDING or FOLLOWING state and may require a follow-up action:

{}

Based on the persona objectives above, decide whether any of these tasks need immediate follow-up attention.

Reply with exactly one of:
- "FOLLOWUP_NEEDED" — if at least one task requires immediate follow-up.
- "NO_FOLLOWUP" — if none of the tasks require immediate attention.

Reply with only one of those two tokens and nothing else."#,
        tasks.join("\n")
    )
}

/// Call the Ollama `/api/generate` endpoint and return the trimmed response text.
async fn call_ollama(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: String,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{base_url}/api/generate");
    let body = OllamaRequest { model, prompt, stream: false };
    let resp: OllamaResponse = client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.response.trim().to_owned())
}

// ── Worker ────────────────────────────────────────────────────────────────────

/// Spawn the follow-up worker.  Runs on a [`WORKER_INTERVAL_SECS`]-second
/// timer and never returns under normal operation.
pub async fn run_worker(tracker: TaskTracker) {
    let client = reqwest::Client::new();
    let ollama_url = std::env::var("OLLAMA_BASE_URL")
        .unwrap_or_else(|_| OLLAMA_BASE_URL.to_string());
    let model = std::env::var("OLLAMA_MODEL")
        .unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(WORKER_INTERVAL_SECS));

    // Skip the first immediate tick so the app finishes initialising first.
    interval.tick().await;

    loop {
        interval.tick().await;

        if let Err(e) = run_once(&client, &ollama_url, &model, &tracker).await {
            eprintln!("[followup] Worker error: {e}");
        }
    }
}

/// Execute one follow-up cycle and return any error encountered.
async fn run_once(
    client: &reqwest::Client,
    ollama_url: &str,
    model: &str,
    tracker: &TaskTracker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Load persona.
    let persona = Persona::load()?;

    // 2. Read last N log entries.
    let entries = tracker.read_last_n(TASK_LOOKBACK)?;

    // 3. Filter to FOLLOWING / PENDING.
    let actionable: Vec<&TaskEntry> = entries
        .iter()
        .filter(|e| {
            matches!(
                e.status.as_deref(),
                Some("FOLLOWING") | Some("PENDING")
            )
        })
        .collect();

    if actionable.is_empty() {
        eprintln!("[followup] No FOLLOWING/PENDING tasks found — skipping LLM call");
        return Ok(());
    }

    eprintln!(
        "[followup] Sending {} actionable task(s) to Ollama model '{model}'",
        actionable.len()
    );

    // 4. Call local LLM.
    let prompt = build_prompt(&persona, &actionable);
    let response = call_ollama(client, ollama_url, model, prompt).await?;

    eprintln!("[followup] Ollama response: {response}");

    // 5. If follow-up is needed, mark all actionable entries.
    if response.contains("FOLLOWUP_NEEDED") {
        let updates: HashMap<String, String> = actionable
            .iter()
            .map(|e| (e.timestamp.clone(), "FOLLOWUP_NEEDED".to_string()))
            .collect();

        tracker.update_statuses(&updates)?;
        eprintln!(
            "[followup] Marked {} task(s) as FOLLOWUP_NEEDED",
            updates.len()
        );
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::{Persona, RoiThresholds, TaskEntry};

    fn make_persona() -> Persona {
        Persona {
            name: "TestBot".into(),
            version: "1.0".into(),
            description: "Test trading agent".into(),
            roi_thresholds: RoiThresholds { min_trigger_usd: 5.0 },
        }
    }

    #[test]
    fn prompt_contains_persona_and_tasks() {
        let persona = make_persona();
        let entry = TaskEntry {
            timestamp: "2024-01-01T00:00:00Z".into(),
            event: "ai_decision".into(),
            persona: "TestBot".into(),
            trigger_remote_ai: Some(true),
            estimated_profit_usd: Some(10.0),
            status: Some("PENDING".into()),
        };
        let entries = vec![&entry];
        let prompt = build_prompt(&persona, &entries);
        assert!(prompt.contains("TestBot"));
        assert!(prompt.contains("PENDING"));
        assert!(prompt.contains("FOLLOWUP_NEEDED"));
        assert!(prompt.contains("NO_FOLLOWUP"));
    }

    #[test]
    fn prompt_contains_persona_even_with_no_entries() {
        let persona = make_persona();
        let prompt = build_prompt(&persona, &[]);
        // Prompt is still well-formed; the tasks section is just blank.
        assert!(prompt.contains("TestBot"));
    }
}
