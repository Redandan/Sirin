//! Background research agent.
//!
//! Runs a multi-step LLM research pipeline on a URL or topic:
//!   1. Fetch & extract page content (if URL given) — [`fetch`]
//!   2. Produce an overview analysis
//!   3. Generate follow-up research questions
//!   4. Search + analyse each question in parallel (one LLM call per question)
//!   5. Synthesize into a final report
//!
//! Steps 2-5 live in [`pipeline`]; [`persistence`] handles the `research.jsonl`
//! log. All intermediate steps are persisted after each phase so the UI and
//! follow-up worker can track progress in real time.

mod fetch;
mod persistence;
mod pipeline;

pub use persistence::{get_research, list_research, save_research};

use std::sync::{Mutex, OnceLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::events;
use crate::sirin_log;

// ── Persona safety gate ──────────────────────────────────────────────────────

/// Proposed objective update waiting for user confirmation in the UI.
/// `maybe_reflect_on_objectives` stores here instead of writing directly.
fn pending_objectives_slot() -> &'static Mutex<Option<Vec<String>>> {
    static SLOT: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Take the pending objectives out of the slot (returns `None` if nothing pending).
/// Called by the UI on each refresh cycle.
pub fn take_pending_objectives() -> Option<Vec<String>> {
    pending_objectives_slot().lock().ok()?.take()
}

pub(super) fn store_pending_objectives(objectives: Vec<String>) {
    if let Ok(mut guard) = pending_objectives_slot().lock() {
        *guard = Some(objectives);
    }
}

// ── Research task types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResearchStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchStep {
    pub phase: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchTask {
    pub id: String,
    pub topic: String,
    pub url: Option<String>,
    pub status: ResearchStatus,
    pub steps: Vec<ResearchStep>,
    pub final_report: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Run the full research pipeline and return the completed task.
///
/// This is designed to be spawned as a background tokio task.
pub async fn run_research(topic: String, url: Option<String>) -> ResearchTask {
    let id = format!("r-{}", Utc::now().timestamp_millis());
    let mut task = ResearchTask {
        id: id.clone(),
        topic: topic.clone(),
        url: url.clone(),
        status: ResearchStatus::Running,
        steps: Vec::new(),
        final_report: None,
        started_at: Utc::now().to_rfc3339(),
        finished_at: None,
    };

    let _ = save_research(&task);

    let scrape_http = fetch::scraping_http();
    let llm_http = crate::llm::shared_http();
    let llm_arc = crate::llm::shared_llm();
    let llm = llm_arc.as_ref();

    // Run the pipeline; on any hard failure record it and return.
    match pipeline::pipeline(scrape_http, &llm_http, llm, &mut task).await {
        Ok(_) => {
            task.status = ResearchStatus::Done;
            task.finished_at = Some(Utc::now().to_rfc3339());
        }
        Err(e) => {
            sirin_log!("[researcher] Pipeline failed for '{}': {e}", task.topic);
            task.steps.push(ResearchStep {
                phase: "error".into(),
                output: e.clone(),
            });
            task.status = ResearchStatus::Failed;
            task.final_report = Some(format!("調研失敗：{e}"));
            task.finished_at = Some(Utc::now().to_rfc3339());
        }
    }

    let _ = save_research(&task);

    // Publish completion event so other agents react immediately.
    events::publish(events::AgentEvent::ResearchCompleted {
        topic: task.topic.clone(),
        task_id: task.id.clone(),
        success: task.status == ResearchStatus::Done,
    });

    // Every 5th successful research task, reflect on persona objectives.
    if task.status == ResearchStatus::Done {
        let done_count = list_research()
            .unwrap_or_default()
            .iter()
            .filter(|t| t.status == ResearchStatus::Done)
            .count();
        if done_count % 5 == 0 {
            pipeline::maybe_reflect_on_objectives(
                crate::llm::shared_http().as_ref(),
                &crate::llm::shared_router_llm(),
                &task,
            )
            .await;
        }
    }

    task
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Full pipeline — requires LM Studio at localhost:1234.
    /// Run with: cargo test pipeline_full -- --nocapture --ignored
    #[tokio::test]
    #[ignore]
    async fn pipeline_full_topic_only() {
        println!("\n======================================");
        println!("🔬 researcher::run_research (topic only)");
        println!("======================================");

        let task = run_research("Rust async/await 底層工作原理".to_string(), None).await;

        println!("  id     = {}", task.id);
        println!("  status = {:?}", task.status);
        println!("  steps  = {}", task.steps.len());
        for s in &task.steps {
            println!("    [{}] {} chars", s.phase, s.output.len());
        }

        assert_ne!(
            task.status,
            ResearchStatus::Failed,
            "pipeline failed: {}",
            task.final_report.as_deref().unwrap_or("")
        );
        assert!(task.final_report.is_some());

        if let Some(report) = &task.final_report {
            println!("\n--- report (first 400 chars) ---");
            println!("{}", &report.chars().take(400).collect::<String>());
        }
        println!("\n✅ pipeline_full_topic_only passed");
    }

    /// Full pipeline with URL — requires LM Studio at localhost:1234.
    /// Run with: cargo test pipeline_url -- --nocapture --ignored
    #[tokio::test]
    #[ignore]
    async fn pipeline_full_with_url() {
        println!("\n======================================");
        println!("🔬 researcher::run_research (URL)");
        println!("======================================");

        let task = run_research(
            "AgoraMarket 平台功能分析".to_string(),
            Some("https://agoramarket.purrtechllc.com/".to_string()),
        )
        .await;

        println!("  id     = {}", task.id);
        println!("  status = {:?}", task.status);
        println!("  steps  = {}", task.steps.len());
        for s in &task.steps {
            println!("    [{}] {} chars", s.phase, s.output.len());
        }

        let has_fetch = task.steps.iter().any(|s| s.phase == "fetch");
        assert!(has_fetch, "fetch phase missing — URL was provided");
        assert_ne!(
            task.status,
            ResearchStatus::Failed,
            "pipeline failed: {}",
            task.final_report.as_deref().unwrap_or("")
        );

        if let Some(report) = &task.final_report {
            println!("\n--- report (first 600 chars) ---");
            println!("{}", &report.chars().take(600).collect::<String>());
        }
        println!("\n✅ pipeline_full_with_url passed");
    }
}
