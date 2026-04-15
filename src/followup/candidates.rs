//! Autonomous candidate selection for the follow-up worker.
//!
//! Reads the recent task log, filters for actionable entries that look like
//! research (or carry a URL), and ranks them by estimated profit + priority
//! flags.  Also enforces concurrency / cooldown / retry limits configured via
//! `AUTONOMOUS_*` env vars.

use crate::persona::TaskEntry;

use super::parse_ts;

/// Max concurrent research tasks for autonomous mode.
const AUTONOMOUS_MAX_CONCURRENT: usize = 2;
/// Max tasks to schedule per worker cycle.
const AUTONOMOUS_MAX_PER_CYCLE: usize = 2;
/// Cooldown window to avoid rescheduling the same source too frequently.
const AUTONOMOUS_COOLDOWN_SECS: i64 = 300;
/// Max retry attempts for the same source task.
const AUTONOMOUS_MAX_RETRIES: usize = 2;

pub(super) fn autonomous_max_concurrent() -> usize {
    std::env::var("AUTONOMOUS_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(AUTONOMOUS_MAX_CONCURRENT)
}

pub(super) fn autonomous_max_per_cycle() -> usize {
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
        .map(|t| t.trim_matches(|c: char| ",.!?)\"'".contains(c)).to_string())
}

pub(super) fn derive_research_plan(entry: &TaskEntry) -> Option<(String, Option<String>)> {
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

pub(super) fn self_assign_candidates(entries: &[TaskEntry]) -> Vec<TaskEntry> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
