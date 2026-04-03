use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use crate::persona::TaskEntry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionRecord {
    pub id: String,
    pub timestamp: String,
    pub source: String,
    pub input: String,
    pub output: String,
    pub latency_ms: u64,
    pub success: bool,
    pub model: Option<String>,
    pub prompt_version: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackRecord {
    pub id: String,
    pub interaction_id: String,
    pub timestamp: String,
    pub rating: i8,
    pub reason: Option<String>,
    pub corrected_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousMetrics {
    pub generated_at: String,
    pub pending_tasks: usize,
    pub followup_needed_tasks: usize,
    pub running_research: usize,
    pub scheduled_last_hour: usize,
    pub completed_success_last_hour: usize,
    pub completed_failed_last_hour: usize,
    pub success_rate_last_hour: f64,
    pub max_concurrent: usize,
    pub max_per_cycle: usize,
    pub cooldown_secs: i64,
    pub max_retries: usize,
}

fn tracking_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return std::path::Path::new(&local_app_data)
            .join("Sirin")
            .join("tracking");
    }

    std::path::Path::new("data").join("tracking")
}

fn interaction_log_path() -> PathBuf {
    tracking_dir().join("interaction.jsonl")
}

fn feedback_log_path() -> PathBuf {
    tracking_dir().join("feedback.jsonl")
}

fn task_log_path() -> PathBuf {
    tracking_dir().join("task.jsonl")
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

fn env_i64(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

fn append_jsonl_line<T: Serialize>(path: &PathBuf, entry: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let line = serde_json::to_string(entry).map_err(|e| e.to_string())?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;

    writeln!(file, "{line}").map_err(|e| e.to_string())
}

pub fn record_interaction(
    source: String,
    input: String,
    output: String,
    latency_ms: u64,
    success: bool,
    model: Option<String>,
    prompt_version: Option<String>,
    metadata: Option<Value>,
) -> Result<String, String> {
    let id = format!("i-{}", Utc::now().timestamp_millis());
    let item = InteractionRecord {
        id: id.clone(),
        timestamp: Utc::now().to_rfc3339(),
        source,
        input,
        output,
        latency_ms,
        success,
        model,
        prompt_version,
        metadata,
    };

    append_jsonl_line(&interaction_log_path(), &item)?;
    Ok(id)
}

pub fn record_feedback(
    interaction_id: String,
    rating: i8,
    reason: Option<String>,
    corrected_output: Option<String>,
) -> Result<String, String> {
    if !(-1..=1).contains(&rating) {
        return Err("rating must be -1, 0, or 1".to_string());
    }

    let id = format!("f-{}", Utc::now().timestamp_millis());
    let item = FeedbackRecord {
        id: id.clone(),
        interaction_id,
        timestamp: Utc::now().to_rfc3339(),
        rating,
        reason,
        corrected_output,
    };

    append_jsonl_line(&feedback_log_path(), &item)?;
    Ok(id)
}

pub fn get_interaction(interaction_id: &str) -> Result<Option<InteractionRecord>, String> {
    let path = interaction_log_path();
    if !path.exists() {
        return Ok(None);
    }

    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let found = BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<InteractionRecord>(&l).ok())
        .find(|item| item.id == interaction_id);

    Ok(found)
}

pub fn read_recent_feedback(limit: usize) -> Result<Vec<FeedbackRecord>, String> {
    let path = feedback_log_path();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut items: Vec<FeedbackRecord> = BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<FeedbackRecord>(&l).ok())
        .collect();

    items.reverse();
    Ok(items.into_iter().take(limit).collect())
}

pub fn read_autonomous_metrics() -> Result<AutonomousMetrics, String> {
    let path = task_log_path();
    let now = Utc::now();

    let mut entries: Vec<TaskEntry> = Vec::new();
    if path.exists() {
        let file = fs::File::open(path).map_err(|e| e.to_string())?;
        entries = BufReader::new(file)
            .lines()
            .filter_map(|l| l.ok())
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<TaskEntry>(&l).ok())
            .collect();
    }

    let pending_tasks = entries
        .iter()
        .filter(|e| e.status.as_deref() == Some("PENDING"))
        .count();

    let followup_needed_tasks = entries
        .iter()
        .filter(|e| e.status.as_deref() == Some("FOLLOWUP_NEEDED"))
        .count();

    let running_research = entries
        .iter()
        .filter(|e| e.event == "autonomous_scheduled" && e.status.as_deref() == Some("FOLLOWING"))
        .count();

    let in_last_hour = |ts: &str| -> bool {
        chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|dt| (now - dt.with_timezone(&Utc)).num_minutes() <= 60)
            .unwrap_or(false)
    };

    let scheduled_last_hour = entries
        .iter()
        .filter(|e| e.event == "autonomous_scheduled" && in_last_hour(&e.timestamp))
        .count();

    let completed_success_last_hour = entries
        .iter()
        .filter(|e| {
            e.event == "autonomous_completed:research"
                && e.status.as_deref() == Some("DONE")
                && in_last_hour(&e.timestamp)
        })
        .count();

    let completed_failed_last_hour = entries
        .iter()
        .filter(|e| {
            e.event == "autonomous_completed:research"
                && e.status.as_deref() == Some("FOLLOWUP_NEEDED")
                && in_last_hour(&e.timestamp)
        })
        .count();

    let completed_total = completed_success_last_hour + completed_failed_last_hour;
    let success_rate_last_hour = if completed_total == 0 {
        1.0
    } else {
        completed_success_last_hour as f64 / completed_total as f64
    };

    Ok(AutonomousMetrics {
        generated_at: now.to_rfc3339(),
        pending_tasks,
        followup_needed_tasks,
        running_research,
        scheduled_last_hour,
        completed_success_last_hour,
        completed_failed_last_hour,
        success_rate_last_hour,
        max_concurrent: env_usize("AUTONOMOUS_MAX_CONCURRENT", 2),
        max_per_cycle: env_usize("AUTONOMOUS_MAX_PER_CYCLE", 2),
        cooldown_secs: env_i64("AUTONOMOUS_COOLDOWN_SECS", 300),
        max_retries: env_usize("AUTONOMOUS_MAX_RETRIES", 2),
    })
}
