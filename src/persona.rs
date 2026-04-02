use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
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
#[derive(Debug, Serialize, Deserialize)]
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
    /// Workflow status for follow-up tracking (e.g. `"PENDING"`, `"FOLLOWING"`, `"FOLLOWUP_NEEDED"`, `"DONE"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
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
            status: None,
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
            status: if triggered { Some("PENDING".to_string()) } else { None },
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

    /// Read and deserialize the last `n` lines of the log file.
    ///
    /// Uses a ring-buffer approach so only `n` lines are held in memory at a
    /// time regardless of file size.  Lines that cannot be deserialized are
    /// silently skipped.  Returns an empty `Vec` when the file does not exist.
    pub fn read_last_n(&self, n: usize) -> Result<Vec<TaskEntry>, Box<dyn std::error::Error + Send + Sync>> {
        let path = self.path.lock().expect("TaskTracker mutex poisoned").clone();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&path)?;
        let reader = BufReader::new(file);

        // Ring-buffer: keep at most n raw lines.
        let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::with_capacity(n);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if ring.len() == n {
                ring.pop_front();
            }
            ring.push_back(line);
        }

        let entries = ring
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Ok(entries)
    }

    /// Rewrite the log file, replacing the `status` field of entries whose
    /// `timestamp` matches a key in `updates`.
    ///
    /// Lines that cannot be parsed are preserved verbatim so no data is lost.
    pub fn update_statuses(
        &self,
        updates: &std::collections::HashMap<String, String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if updates.is_empty() {
            return Ok(());
        }
        let path = self.path.lock().expect("TaskTracker mutex poisoned").clone();
        if !path.exists() {
            return Ok(());
        }

        // Read all existing lines.
        let raw: Vec<String> = {
            let file = fs::File::open(&path)?;
            BufReader::new(file)
                .lines()
                .filter_map(|l| l.ok())
                .collect()
        };

        // Rewrite with updated statuses where applicable.
        let tmp_path = path.with_extension("jsonl.tmp");
        {
            let mut tmp = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            for line in &raw {
                if line.trim().is_empty() {
                    writeln!(tmp, "{line}")?;
                    continue;
                }
                if let Ok(mut entry) = serde_json::from_str::<TaskEntry>(line) {
                    if let Some(new_status) = updates.get(&entry.timestamp) {
                        entry.status = Some(new_status.clone());
                        writeln!(tmp, "{}", serde_json::to_string(&entry)?)?;
                        continue;
                    }
                }
                writeln!(tmp, "{line}")?;
            }
        }

        // Atomically replace original file with updated one.
        fs::rename(&tmp_path, &path)?;
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
