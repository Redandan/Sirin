use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ── ROI thresholds ────────────────────────────────────────────────────────────

/// Profit thresholds that drive AI-invocation decisions.
#[derive(Debug, Clone, Deserialize)]
pub struct RoiThresholds {
    /// Minimum estimated profit (USD) required to trigger the remote AI call.
    pub min_trigger_usd: f64,
}

// ── Persona ───────────────────────────────────────────────────────────────────

/// Identity and decision parameters loaded from `config/persona.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Persona {
    pub name: String,
    pub version: String,
    pub description: String,
    pub roi_thresholds: RoiThresholds,
}

impl Persona {
    /// Read and deserialize `config/persona.yaml` from the working directory.
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = fs::read_to_string("config/persona.yaml")?;
        let persona = serde_yaml::from_str(&content)?;
        Ok(persona)
    }

    /// Returns `true` only when `estimated_profit` strictly exceeds
    /// [`RoiThresholds::min_trigger_usd`].
    pub fn should_trigger_remote_ai(&self, estimated_profit: f64) -> bool {
        estimated_profit > self.roi_thresholds.min_trigger_usd
    }
}

// ── Task log entry ────────────────────────────────────────────────────────────

/// A single decision or activity record serialized as one JSON line.
#[derive(Debug, Serialize)]
pub struct TaskEntry {
    /// RFC 3339 timestamp of when the entry was created.
    pub timestamp: String,
    /// Short label describing the event (e.g. `"heartbeat"`, `"ai_decision"`).
    pub event: String,
    /// Name of the active persona at the time of the event.
    pub persona: String,
    /// Whether the remote-AI trigger fired for this decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_remote_ai: Option<bool>,
    /// Estimated profit that drove the decision, in USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_profit_usd: Option<f64>,
}

impl TaskEntry {
    /// Convenience constructor for a plain heartbeat entry.
    pub fn heartbeat(persona_name: &str) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "heartbeat".to_string(),
            persona: persona_name.to_string(),
            trigger_remote_ai: None,
            estimated_profit_usd: None,
        }
    }

    /// Convenience constructor for an AI-decision entry.
    pub fn ai_decision(persona_name: &str, estimated_profit: f64, triggered: bool) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "ai_decision".to_string(),
            persona: persona_name.to_string(),
            trigger_remote_ai: Some(triggered),
            estimated_profit_usd: Some(estimated_profit),
        }
    }
}

// ── TaskTracker ───────────────────────────────────────────────────────────────

/// Thread-safe appender that writes [`TaskEntry`] records as JSON lines to
/// `data/tracking/task.jsonl`.
///
/// Uses an [`Arc`]`<`[`Mutex`]`>` so the tracker can be cheaply cloned and
/// shared across threads / async tasks without interleaving writes.
#[derive(Clone)]
pub struct TaskTracker {
    path: Arc<Mutex<PathBuf>>,
}

impl TaskTracker {
    /// Create a tracker that writes to the given file path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(Mutex::new(path.into())),
        }
    }

    /// Serialize `entry` as a JSON line and append it to the log file.
    ///
    /// Acquires the internal mutex so that concurrent callers never interleave
    /// their writes.  The parent directory is created automatically if it does
    /// not yet exist.
    pub fn record(&self, entry: &TaskEntry) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let path = self.path.lock().expect("TaskTracker mutex poisoned");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(entry)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_above_threshold() {
        let persona = Persona {
            name: "Test".into(),
            version: "1.0".into(),
            description: "".into(),
            roi_thresholds: RoiThresholds { min_trigger_usd: 5.0 },
        };
        assert!(persona.should_trigger_remote_ai(5.01));
        assert!(persona.should_trigger_remote_ai(100.0));
    }

    #[test]
    fn no_trigger_at_or_below_threshold() {
        let persona = Persona {
            name: "Test".into(),
            version: "1.0".into(),
            description: "".into(),
            roi_thresholds: RoiThresholds { min_trigger_usd: 5.0 },
        };
        assert!(!persona.should_trigger_remote_ai(5.0));
        assert!(!persona.should_trigger_remote_ai(0.0));
        assert!(!persona.should_trigger_remote_ai(-1.0));
    }

    #[test]
    fn task_tracker_writes_jsonl() {
        let dir = std::env::temp_dir().join(format!("sirin_test_{}", std::process::id()));
        let path = dir.join("task.jsonl");
        let tracker = TaskTracker::new(&path);

        let entry = TaskEntry::heartbeat("TestPersona");
        tracker.record(&entry).expect("record failed");

        let content = fs::read_to_string(&path).expect("file missing");
        assert!(content.contains("\"event\":\"heartbeat\""));
        assert!(content.contains("\"persona\":\"TestPersona\""));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn task_tracker_appends_multiple_lines() {
        let dir = std::env::temp_dir().join(format!("sirin_test_multi_{}", std::process::id()));
        let path = dir.join("task.jsonl");
        let tracker = TaskTracker::new(&path);

        tracker.record(&TaskEntry::heartbeat("P1")).unwrap();
        tracker.record(&TaskEntry::ai_decision("P1", 10.0, true)).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);

        fs::remove_dir_all(&dir).ok();
    }
}
