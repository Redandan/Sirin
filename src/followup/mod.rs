//! Background follow-up worker for Sirin.
//!
//! Runs near-continuously (every [`WORKER_INTERVAL_SECS`] seconds, or on
//! `ResearchCompleted` / `FollowupTriggered` events).  Each run:
//!
//! 1. Reads the last [`TASK_LOOKBACK`] lines from `data/tracking/task.jsonl`.
//! 2. Asks [`candidates::self_assign_candidates`] which entries look like
//!    research worth running autonomously (respecting concurrency limits).
//! 3. Applies rule-based follow-up classification to anything still
//!    `FOLLOWING` / `PENDING` (no LLM call for this binary decision).
//! 4. Publishes `FollowupTriggered` events and trims the log periodically.

mod candidates;

use std::collections::HashMap;

use crate::events;
use crate::persona::{TaskEntry, TaskTracker};
use crate::researcher::{self, ResearchStatus};
use crate::sirin_log;

use candidates::{autonomous_max_concurrent, autonomous_max_per_cycle, derive_research_plan, self_assign_candidates};

// ── Constants / env ──────────────────────────────────────────────────────────

/// How many trailing log lines to inspect on each run.
const TASK_LOOKBACK: usize = 50;

/// Interval between worker runs (near real-time).
const WORKER_INTERVAL_SECS: u64 = 20;

/// Stale-PENDING threshold — PENDING tasks older than this are forced to follow-up.
const STALE_PENDING_SECS: i64 = 3600; // 1 h

fn worker_interval_secs() -> u64 {
    std::env::var("FOLLOWUP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(WORKER_INTERVAL_SECS)
}

/// How many log entries to retain after each trim (configurable via env var).
fn task_log_max_lines() -> usize {
    std::env::var("TASK_LOG_MAX_LINES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(2000)
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Parse an RFC3339 timestamp to UTC.  Shared with the candidates submodule.
pub(super) fn parse_ts(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

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

/// Rule-based decision: does any actionable task need follow-up right now?
///
/// Rules (OR-combined):
/// 1. Already explicitly marked `FOLLOWUP_NEEDED`.
/// 2. Stale `PENDING` task — no update for more than `STALE_PENDING_SECS`.
/// 3. `high_priority` flag is set.
fn should_followup_now(actionable: &[&TaskEntry]) -> bool {
    actionable.iter().any(|e| {
        e.status.as_deref() == Some("FOLLOWUP_NEEDED")
            || e.high_priority == Some(true)
            || (e.status.as_deref() == Some("PENDING") && is_stale(e, STALE_PENDING_SECS))
    })
}

fn is_stale(entry: &TaskEntry, max_age_secs: i64) -> bool {
    parse_ts(&entry.timestamp)
        .map(|ts| (chrono::Utc::now() - ts).num_seconds() > max_age_secs)
        .unwrap_or(false)
}

// ── Worker ───────────────────────────────────────────────────────────────────

/// Spawn the follow-up worker.  Runs on a [`WORKER_INTERVAL_SECS`]-second
/// timer and never returns under normal operation.
pub async fn run_worker(tracker: TaskTracker) {
    let interval_secs = worker_interval_secs();

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));

    // Subscribe to the event bus so we react immediately to ResearchCompleted.
    let mut event_rx = events::subscribe();

    // Skip the first immediate tick so the app finishes initialising first.
    interval.tick().await;

    let mut cycle: u32 = 0;

    loop {
        // Wait for either a timed tick OR an event that warrants an early cycle.
        let triggered_by_event = tokio::select! {
            _ = interval.tick() => false,
            event = event_rx.recv() => {
                match event {
                    Ok(events::AgentEvent::ResearchCompleted { topic, task_id, success }) => {
                        sirin_log!(
                            "[followup] Event-driven cycle: ResearchCompleted topic={topic} id={task_id} ok={success}"
                        );
                        true
                    }
                    Ok(events::AgentEvent::FollowupTriggered { source_timestamp }) => {
                        sirin_log!(
                            "[followup] Event-driven cycle: FollowupTriggered ts={source_timestamp}"
                        );
                        true
                    }
                    _ => false,
                }
            }
        };

        cycle += 1;

        if triggered_by_event {
            sirin_log!("[followup] Running event-triggered cycle #{cycle}");
        }

        if let Err(e) = run_once(&tracker).await {
            sirin_log!("[followup] Worker error: {e}");
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
                Ok(n) => {
                    sirin_log!("[followup] Task log trimmed: removed {n} old entries (max={max})")
                }
                Err(e) => sirin_log!("[followup] Task log trim failed: {e}"),
            }
        }
    }
}

/// Execute one follow-up cycle and return any error encountered.
async fn run_once(tracker: &TaskTracker) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Read last N log entries.
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
            sirin_log!(
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

        sirin_log!(
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
                    let task = crate::agents::research_agent::run_research_via_adk_with_tracker(
                        topic,
                        url,
                        Some(tracker_clone.clone()),
                    )
                    .await;

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
                    final_updates.insert(source_ts, done_status.clone());
                    let _ = tracker_clone.update_statuses(&final_updates);

                    // Notify event bus so other agents react immediately.
                    events::publish(events::AgentEvent::ResearchCompleted {
                        topic: task.topic.clone(),
                        task_id: task.id.clone(),
                        success: done_status == "DONE",
                    });
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
        .filter(|e| matches!(e.status.as_deref(), Some("FOLLOWING") | Some("PENDING")))
        .collect();

    if actionable.is_empty() {
        return Ok(());
    }

    sirin_log!(
        "[followup] Evaluating {} actionable task(s) with rule-based logic",
        actionable.len()
    );

    // 4. Rule-based follow-up decision (no LLM needed for binary classify).
    if should_followup_now(&actionable) {
        if let Some(primary) = actionable.first() {
            let mut updates = HashMap::new();
            updates.insert(primary.timestamp.clone(), "FOLLOWUP_NEEDED".to_string());

            tracker.update_statuses(&updates)?;
            record_optimization_log(
                tracker,
                "optimization_followup_marked",
                Some("marked 1 task FOLLOWUP_NEEDED (rule-based)".to_string()),
                Some("FOLLOWUP_NEEDED"),
                None,
                correlation_id_for(primary),
            );
            sirin_log!(
                "[followup] Marked task {} as FOLLOWUP_NEEDED (rule)",
                primary.timestamp
            );

            // Notify subscribers via event bus.
            events::publish(events::AgentEvent::FollowupTriggered {
                source_timestamp: primary.timestamp.clone(),
            });
        }
    } else {
        sirin_log!("[followup] Rules: no immediate follow-up needed this cycle");
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_persona_and_tasks() {
        // Replaced build_prompt with rule-based should_followup_now.
        let entry = TaskEntry {
            timestamp: "2024-01-01T00:00:00Z".into(),
            event: "ai_decision".into(),
            persona: "TestBot".into(),
            correlation_id: None,
            message_preview: Some("Monitor Agora signal and respond".into()),
            trigger_remote_ai: Some(true),
            estimated_profit_usd: Some(10.0),
            status: Some("FOLLOWUP_NEEDED".into()),
            reason: None,
            action_tier: None,
            high_priority: None,
        };
        // A FOLLOWUP_NEEDED entry must trigger.
        assert!(should_followup_now(&[&entry]));
    }

    #[test]
    fn prompt_contains_persona_even_with_no_entries() {
        // No actionable entries → no followup.
        assert!(!should_followup_now(&[]));
    }
}
