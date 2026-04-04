//! Background follow-up worker for Sirin.
//!
//! Runs every 30 minutes (or whenever the Tokio runtime is otherwise idle).
//! Each run:
//!
//! 1. Reads the last [`TASK_LOOKBACK`] lines from `data/tracking/task.jsonl`.
//! 2. Filters entries whose `status` is `"FOLLOWING"` or `"PENDING"`.
//! 3. Builds a prompt from the active [`Persona`] objectives + the filtered
//!    entries and sends it to a local LLM backend (Ollama or LM Studio).
//! 4. If the model responds that a follow-up is needed, updates those entries'
//!    status to `"FOLLOWUP_NEEDED"` in the JSONL file.

use std::collections::HashMap;

use crate::llm::{call_prompt, LlmConfig};
use crate::persona::{Persona, TaskEntry, TaskTracker};
use crate::researcher::{self, ResearchStatus};

fn record_optimization_log(
    tracker: &TaskTracker,
    event: &str,
    message_preview: Option<String>,
    status: Option<&str>,
    reason: Option<String>,
    correlation_id: Option<String>,
) {
    let entry = TaskEntry::system_event(
        "Sirin",
        event,
        message_preview,
        status,
        reason,
        correlation_id,
    );
    let _ = tracker.record(&entry);
}

fn correlation_id_for(entry: &TaskEntry) -> Option<String> {
    entry.correlation_id.clone().or_else(|| {
        entry
            .reason
            .as_deref()
            .and_then(|reason| reason.strip_prefix("feedback_id="))
            .map(|value| value.to_string())
    })
}

/// How many trailing log lines to inspect on each run.
const TASK_LOOKBACK: usize = 50;

/// Interval between worker runs (near real-time).
const WORKER_INTERVAL_SECS: u64 = 20;
/// Max concurrent research tasks for autonomous mode.
const AUTONOMOUS_MAX_CONCURRENT: usize = 2;
/// Max tasks to schedule per worker cycle.
const AUTONOMOUS_MAX_PER_CYCLE: usize = 2;
/// Cooldown window to avoid rescheduling the same source too frequently.
const AUTONOMOUS_COOLDOWN_SECS: i64 = 300;
/// Max retry attempts for the same source task.
const AUTONOMOUS_MAX_RETRIES: usize = 2;

fn worker_interval_secs() -> u64 {
    std::env::var("FOLLOWUP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(WORKER_INTERVAL_SECS)
}

fn autonomous_max_concurrent() -> usize {
    std::env::var("AUTONOMOUS_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(AUTONOMOUS_MAX_CONCURRENT)
}

fn autonomous_max_per_cycle() -> usize {
    std::env::var("AUTONOMOUS_MAX_PER_CYCLE")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(AUTONOMOUS_MAX_PER_CYCLE)
}

fn autonomous_cooldown_secs() -> i64 {
    std::env::var("AUTONOMOUS_COOLDOWN_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(AUTONOMOUS_COOLDOWN_SECS)
}

fn autonomous_max_retries() -> usize {
    std::env::var("AUTONOMOUS_MAX_RETRIES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(AUTONOMOUS_MAX_RETRIES)
}

fn first_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|t| t.starts_with("https://") || t.starts_with("http://"))
        .map(|t| {
            t.trim_matches(|c: char| ",.!?)\"'".contains(c))
                .to_string()
        })
}

fn derive_research_plan(entry: &TaskEntry) -> Option<(String, Option<String>)> {
    let text = entry.message_preview.as_deref()?.trim();
    if text.is_empty() {
        return None;
    }

    // Ignore machine-generated metadata lines to avoid recursive self-scheduling.
    if text.starts_with("source=") {
        return None;
    }

    let looks_like_research = ["調研", "研究", "查資料", "分析", "investigate", "research"]
        .iter()
        .any(|kw| text.to_lowercase().contains(&kw.to_lowercase()));

    let url = first_url(text);

    if !looks_like_research && url.is_none() {
        return None;
    }

    let topic = text.replace("\n", " ").trim().to_string();
    Some((topic, url))
}

fn parse_ts(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

fn is_system_generated_event(entry: &TaskEntry) -> bool {
    entry.event.starts_with("autonomous_")
}

fn has_active_schedule(entries: &[TaskEntry], source_timestamp: &str) -> bool {
    entries.iter().any(|e| {
        e.event == "autonomous_scheduled"
            && e.reason.as_deref() == Some(source_timestamp)
            && e.status.as_deref() == Some("FOLLOWING")
    })
}

fn failure_count(entries: &[TaskEntry], source_timestamp: &str) -> usize {
    entries
        .iter()
        .filter(|e| {
            e.event == "autonomous_completed:research"
                && e.reason.as_deref() == Some(source_timestamp)
                && e.status.as_deref() == Some("FOLLOWUP_NEEDED")
        })
        .count()
}

fn in_cooldown(entries: &[TaskEntry], source_timestamp: &str, cooldown_secs: i64) -> bool {
    let latest = entries
        .iter()
        .filter(|e| {
            (e.event == "autonomous_scheduled" || e.event == "autonomous_completed:research")
                && e.reason.as_deref() == Some(source_timestamp)
        })
        .filter_map(|e| parse_ts(&e.timestamp))
        .max();

    if let Some(last_ts) = latest {
        return (chrono::Utc::now() - last_ts).num_seconds() < cooldown_secs;
    }

    false
}

fn self_assign_candidates(entries: &[TaskEntry]) -> Vec<TaskEntry> {
    let cooldown_secs = autonomous_cooldown_secs();
    let max_retries = autonomous_max_retries();

    let mut candidates: Vec<TaskEntry> = entries
        .iter()
        .filter(|e| {
            matches!(
                e.status.as_deref(),
                Some("FOLLOWUP_NEEDED") | Some("PENDING")
            )
        })
        .filter(|e| !is_system_generated_event(e))
        .filter(|e| !has_active_schedule(entries, &e.timestamp))
        .filter(|e| failure_count(entries, &e.timestamp) < max_retries)
        .filter(|e| !in_cooldown(entries, &e.timestamp, cooldown_secs))
        .filter_map(|e| {
            if derive_research_plan(e).is_some() {
                Some(e.clone())
            } else {
                None
            }
        })
        .collect();

    candidates.sort_by(|a, b| {
        let a_score = candidate_priority(a);
        let b_score = candidate_priority(b);
        b_score.total_cmp(&a_score)
    });

    candidates
}

fn candidate_priority(entry: &TaskEntry) -> f64 {
    let mut score = entry.estimated_profit_usd.unwrap_or(0.0);

    if entry.status.as_deref() == Some("FOLLOWUP_NEEDED") {
        score += 100.0;
    }

    if entry.high_priority == Some(true) {
        score += 50.0;
    }

    if let Some(text) = entry.message_preview.as_deref() {
        if first_url(text).is_some() {
            score += 20.0;
        }
    }

    score
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build the prompt text sent to the local LLM.
fn build_prompt(persona: &Persona, entries: &[&TaskEntry]) -> String {
    let objectives = format!(
        "Persona: {} (v{})\nDescription: {}\nROI threshold: ${:.2} USD",
        persona.name(),
        persona.version,
        persona.description,
        persona.roi_thresholds.min_usd_to_call_remote_llm
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

// ── Worker ────────────────────────────────────────────────────────────────────

/// Spawn the follow-up worker.  Runs on a [`WORKER_INTERVAL_SECS`]-second
/// timer and never returns under normal operation.
/// How many log entries to retain after each trim (configurable via env var).
fn task_log_max_lines() -> usize {
    std::env::var("TASK_LOG_MAX_LINES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(2000)
}

pub async fn run_worker(tracker: TaskTracker) {
    let client = reqwest::Client::new();
    let llm = LlmConfig::from_env();
    let interval_secs = worker_interval_secs();

    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(interval_secs));

    // Skip the first immediate tick so the app finishes initialising first.
    interval.tick().await;

    let mut cycle: u32 = 0;

    loop {
        interval.tick().await;
        cycle += 1;

        if let Err(e) = run_once(&client, &llm, &tracker).await {
            eprintln!("[followup] Worker error: {e}");
            record_optimization_log(
                &tracker,
                "optimization_cycle_error",
                Some(e.to_string()),
                Some("FAILED"),
                None,
                None,
            );
        }

        // Trim the task log every 10 cycles (~5 h at the default interval).
        if cycle % 10 == 0 {
            let max = task_log_max_lines();
            match tracker.trim_to_max(max) {
                Ok(0) => {}
                Ok(n) => eprintln!("[followup] Task log trimmed: removed {n} old entries (max={max})"),
                Err(e) => eprintln!("[followup] Task log trim failed: {e}"),
            }
        }
    }
}

/// Execute one follow-up cycle and return any error encountered.
async fn run_once(
    client: &reqwest::Client,
    llm: &LlmConfig,
    tracker: &TaskTracker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Load persona.
    let persona = Persona::load()?;

    // 2. Read last N log entries.
    let entries = tracker.read_last_n(TASK_LOOKBACK)?;

    // 2.5 If tasks are already marked follow-up/pending and look like research,
    // self-assign immediately without waiting for another planning cycle.
    let candidates = self_assign_candidates(&entries);
    if !candidates.is_empty() {
        let running_count = researcher::list_research()?
            .into_iter()
            .filter(|t| t.status == ResearchStatus::Running)
            .count();

        let max_concurrent = autonomous_max_concurrent();
        let slots = max_concurrent.saturating_sub(running_count);
        let per_cycle = autonomous_max_per_cycle();
        let schedule_cap = slots.min(per_cycle);

        if schedule_cap == 0 {
            eprintln!(
                "[followup] Autonomous queue is full (running={running_count}, max={max_concurrent})"
            );
            record_optimization_log(
                tracker,
                "optimization_deferred",
                Some(format!(
                    "queue full running={} max_concurrent={} candidates={}",
                    running_count,
                    max_concurrent,
                    candidates.len()
                )),
                Some("DEFERRED"),
                None,
                candidates.first().and_then(correlation_id_for),
            );
            return Ok(());
        }

        eprintln!(
            "[followup] Self-assigning up to {} of {} candidate task(s) for autonomous research",
            schedule_cap,
            candidates.len()
        );

        let mut updates = HashMap::new();

        for c in candidates.into_iter().take(schedule_cap) {
            if let Some((topic, url)) = derive_research_plan(&c) {
                let source_ts = c.timestamp.clone();
                let correlation_id = correlation_id_for(&c);

                record_optimization_log(
                    tracker,
                    "optimization_candidate_selected",
                    Some(format!("source={} topic={}", source_ts, topic)),
                    Some("SELECTED"),
                    Some(c.event.clone()),
                    correlation_id.clone(),
                );

                let scheduled = TaskEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    event: "autonomous_scheduled".to_string(),
                    persona: "Sirin".to_string(),
                    correlation_id: correlation_id.clone(),
                    message_preview: Some(format!("source={source_ts} topic={topic}")),
                    trigger_remote_ai: None,
                    estimated_profit_usd: None,
                    status: Some("RUNNING".to_string()),
                    reason: Some(source_ts.clone()),
                    action_tier: None,
                    high_priority: None,
                };
                tracker.record(&scheduled)?;

                updates.insert(source_ts.clone(), "RUNNING".to_string());

                let tracker_clone = tracker.clone();
                tokio::spawn(async move {
                    let task = researcher::run_research(topic, url).await;

                    let done_status = match task.status {
                        ResearchStatus::Done => "DONE",
                        _ => "FOLLOWUP_NEEDED",
                    }
                    .to_string();

                    let completion = TaskEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        event: "autonomous_completed:research".to_string(),
                        persona: "Sirin".to_string(),
                        correlation_id,
                        message_preview: Some(format!(
                            "source={} research_id={} status={:?}",
                            source_ts, task.id, task.status
                        )),
                        trigger_remote_ai: None,
                        estimated_profit_usd: None,
                        status: Some(done_status.clone()),
                        reason: Some(source_ts.clone()),
                        action_tier: None,
                        high_priority: None,
                    };

                    let _ = tracker_clone.record(&completion);
                    let mut final_updates = HashMap::new();
                    final_updates.insert(source_ts, done_status);
                    let _ = tracker_clone.update_statuses(&final_updates);
                });
            }
        }

        if !updates.is_empty() {
            tracker.update_statuses(&updates)?;
        }
    }

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
        record_optimization_log(
            tracker,
            "optimization_cycle_idle",
            Some("no actionable FOLLOWING/PENDING tasks".to_string()),
            Some("IDLE"),
            None,
            None,
        );
        return Ok(());
    }

    eprintln!(
        "[followup] Sending {} actionable task(s) to {} model '{}'",
        actionable.len(),
        llm.backend_name(),
        llm.model
    );

    // 4. Call local LLM.
    let prompt = build_prompt(&persona, &actionable);
    let response = call_prompt(client, llm, prompt).await?;

    eprintln!("[followup] LLM response: {response}");

    // 5. If follow-up is needed, mark only the primary actionable entry.
    // This avoids bulk-flipping unrelated tasks in the same cycle.
    if response.contains("FOLLOWUP_NEEDED") {
        if let Some(primary) = actionable.first() {
            let mut updates = HashMap::new();
            updates.insert(primary.timestamp.clone(), "FOLLOWUP_NEEDED".to_string());

            tracker.update_statuses(&updates)?;
            record_optimization_log(
                tracker,
                "optimization_followup_marked",
                Some("marked 1 task FOLLOWUP_NEEDED".to_string()),
                Some("FOLLOWUP_NEEDED"),
                None,
                correlation_id_for(primary),
            );
            eprintln!(
                "[followup] Marked primary task {} as FOLLOWUP_NEEDED",
                primary.timestamp
            );
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::{Identity, Persona, ProfessionalTone, ResponseStyle, RoiThresholds, TaskEntry};

    fn make_persona() -> Persona {
        Persona {
            identity: Identity {
                name: "TestBot".into(),
                professional_tone: ProfessionalTone::Detailed,
            },
            objectives: vec!["Monitor Agora".into()],
            version: "1.0".into(),
            description: "Test trading agent".into(),
            roi_thresholds: RoiThresholds {
                min_usd_to_notify: 5.0,
                min_usd_to_call_remote_llm: 25.0,
            },
            response_style: ResponseStyle::default(),
        }
    }

    #[test]
    fn prompt_contains_persona_and_tasks() {
        let persona = make_persona();
        let entry = TaskEntry {
            timestamp: "2024-01-01T00:00:00Z".into(),
            event: "ai_decision".into(),
            persona: "TestBot".into(),
            correlation_id: None,
            message_preview: Some("Monitor Agora signal and respond".into()),
            trigger_remote_ai: Some(true),
            estimated_profit_usd: Some(10.0),
            status: Some("PENDING".into()),
            reason: None,
            action_tier: None,
            high_priority: None,
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

    #[test]
    fn derive_research_plan_detects_keywords_and_url() {
        let entry = TaskEntry {
            timestamp: "2024-01-01T00:00:00Z".into(),
            event: "user_request".into(),
            persona: "TestBot".into(),
            correlation_id: None,
            message_preview: Some("請幫我調研 https://example.com 的產品定位".into()),
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: Some("PENDING".into()),
            reason: None,
            action_tier: None,
            high_priority: None,
        };

        let plan = derive_research_plan(&entry);
        assert!(plan.is_some());
        let (_topic, url) = plan.expect("plan should exist");
        assert_eq!(url.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn derive_research_plan_rejects_non_research_text() {
        let entry = TaskEntry {
            timestamp: "2024-01-01T00:00:00Z".into(),
            event: "user_request".into(),
            persona: "TestBot".into(),
            correlation_id: None,
            message_preview: Some("今天天氣不錯".into()),
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: Some("PENDING".into()),
            reason: None,
            action_tier: None,
            high_priority: None,
        };

        assert!(derive_research_plan(&entry).is_none());
    }

    #[test]
    fn self_assign_candidates_prioritizes_followup_needed_and_high_value() {
        let low = TaskEntry {
            timestamp: "2024-01-01T00:00:00Z".into(),
            event: "user_request".into(),
            persona: "TestBot".into(),
            correlation_id: None,
            message_preview: Some("請幫我調研這個主題".into()),
            trigger_remote_ai: None,
            estimated_profit_usd: Some(1.0),
            status: Some("PENDING".into()),
            reason: None,
            action_tier: None,
            high_priority: None,
        };

        let high = TaskEntry {
            timestamp: "2024-01-01T00:00:01Z".into(),
            event: "user_request".into(),
            persona: "TestBot".into(),
            correlation_id: None,
            message_preview: Some("請分析 https://example.com".into()),
            trigger_remote_ai: None,
            estimated_profit_usd: Some(10.0),
            status: Some("FOLLOWUP_NEEDED".into()),
            reason: None,
            action_tier: None,
            high_priority: Some(true),
        };

        let ordered = self_assign_candidates(&[low.clone(), high.clone()]);
        assert_eq!(ordered[0].timestamp, high.timestamp);
        assert_eq!(ordered[1].timestamp, low.timestamp);
    }

    #[test]
    fn in_cooldown_detects_recent_schedule() {
        let now = chrono::Utc::now();
        let recent = TaskEntry {
            timestamp: now.to_rfc3339(),
            event: "autonomous_scheduled".into(),
            persona: "Sirin".into(),
            correlation_id: None,
            message_preview: None,
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: Some("FOLLOWING".into()),
            reason: Some("src-1".into()),
            action_tier: None,
            high_priority: None,
        };

        assert!(in_cooldown(&[recent], "src-1", 300));
    }

    #[test]
    fn failure_count_counts_failed_autonomous_runs() {
        let failed = TaskEntry {
            timestamp: "2024-01-01T00:00:00Z".into(),
            event: "autonomous_completed:research".into(),
            persona: "Sirin".into(),
            correlation_id: None,
            message_preview: None,
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: Some("FOLLOWUP_NEEDED".into()),
            reason: Some("src-1".into()),
            action_tier: None,
            high_priority: None,
        };

        assert_eq!(failure_count(&[failed], "src-1"), 1);
    }
}
