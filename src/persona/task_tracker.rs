//! Task event log — append-only JSONL with atomic update / trim / lookup.
//!
//! `TaskTracker` wraps a single file (typically `{app_data}/tracking/task.jsonl`)
//! that all agents append `TaskEntry` records to.  The UI reads the tail via
//! `read_last_n` for the task board; the followup worker calls
//! `update_statuses` to mark entries `FOLLOWING` / `DONE`.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::{ActionTier, BehaviorDecision, Persona};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEntry {
    pub timestamp: String,
    pub event: String,
    pub persona: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_remote_ai: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_profit_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_tier: Option<ActionTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_priority: Option<bool>,
}

impl TaskEntry {
    pub fn heartbeat(persona_name: &str) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "heartbeat".to_string(),
            persona: persona_name.to_string(),
            correlation_id: None,
            message_preview: None,
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: None,
            reason: None,
            action_tier: None,
            high_priority: None,
        }
    }

    pub fn ai_decision(persona_name: &str, message_preview: Option<String>) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "ai_decision".to_string(),
            persona: persona_name.to_string(),
            correlation_id: None,
            message_preview,
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: None,
            reason: None,
            action_tier: None,
            high_priority: None,
        }
    }

    pub fn behavior_decision(
        persona: &Persona,
        estimated_value: f64,
        decision: &BehaviorDecision,
    ) -> Self {
        let status = match decision.tier {
            ActionTier::Ignore => Some("DONE".to_string()),
            ActionTier::LocalProcess => Some("FOLLOWING".to_string()),
            ActionTier::Escalate => Some("PENDING".to_string()),
        };

        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "behavior_decision".to_string(),
            persona: persona.name().to_string(),
            correlation_id: None,
            message_preview: None,
            trigger_remote_ai: Some(matches!(decision.tier, ActionTier::Escalate)),
            estimated_profit_usd: Some(estimated_value),
            status,
            reason: Some(decision.reason.clone()),
            action_tier: Some(decision.tier),
            high_priority: Some(decision.high_priority),
        }
    }

    pub fn system_event(
        persona_name: &str,
        event: impl Into<String>,
        message_preview: Option<String>,
        status: Option<&str>,
        reason: Option<String>,
        correlation_id: Option<String>,
    ) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: event.into(),
            persona: persona_name.to_string(),
            correlation_id,
            message_preview,
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: status.map(|s| s.to_string()),
            reason,
            action_tier: None,
            high_priority: None,
        }
    }
}

#[derive(Clone)]
pub struct TaskTracker {
    path: Arc<Mutex<PathBuf>>,
}

impl TaskTracker {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(Mutex::new(path.into())),
        }
    }

    pub fn record(
        &self,
        entry: &TaskEntry,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let path = self.path.lock().expect("TaskTracker mutex poisoned");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(entry)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&*path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    fn read_raw_lines_lossy(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let path = self
            .path
            .lock()
            .expect("TaskTracker mutex poisoned")
            .clone();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&path)?;
        let mut reader = BufReader::new(file);
        let mut lines = Vec::new();
        let mut buf = Vec::new();

        loop {
            buf.clear();
            let bytes = reader.read_until(b'\n', &mut buf)?;
            if bytes == 0 {
                break;
            }

            if matches!(buf.last(), Some(b'\n')) {
                buf.pop();
            }
            if matches!(buf.last(), Some(b'\r')) {
                buf.pop();
            }

            lines.push(String::from_utf8_lossy(&buf).into_owned());
        }

        Ok(lines)
    }

    pub fn read_last_n(
        &self,
        n: usize,
    ) -> Result<Vec<TaskEntry>, Box<dyn std::error::Error + Send + Sync>> {
        let mut ring: std::collections::VecDeque<String> =
            std::collections::VecDeque::with_capacity(n);
        for line in self.read_raw_lines_lossy()? {
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

    pub fn update_statuses(
        &self,
        updates: &HashMap<String, String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if updates.is_empty() {
            return Ok(());
        }
        let path = self
            .path
            .lock()
            .expect("TaskTracker mutex poisoned")
            .clone();
        if !path.exists() {
            return Ok(());
        }

        let raw = self.read_raw_lines_lossy()?;

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

        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    pub fn find_by_timestamp(
        &self,
        timestamp: &str,
    ) -> Result<Option<TaskEntry>, Box<dyn std::error::Error + Send + Sync>> {
        for line in self.read_raw_lines_lossy()? {
            if line.trim().is_empty() {
                continue;
            }

            if let Ok(entry) = serde_json::from_str::<TaskEntry>(&line) {
                if entry.timestamp == timestamp {
                    return Ok(Some(entry));
                }
            }
        }

        Ok(None)
    }

    /// Keep only the newest `max_lines` entries, discarding the oldest.
    ///
    /// Returns the number of entries removed, or `0` if no trim was needed.
    /// The file is rewritten atomically via a `.tmp` swap.
    pub fn trim_to_max(
        &self,
        max_lines: usize,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let all = self.read_raw_lines_lossy()?;
        let non_empty: Vec<&str> = all
            .iter()
            .map(|l| l.as_str())
            .filter(|l| !l.trim().is_empty())
            .collect();

        if non_empty.len() <= max_lines {
            return Ok(0);
        }

        let removed = non_empty.len() - max_lines;
        let keep = &non_empty[removed..];

        let path = self
            .path
            .lock()
            .expect("TaskTracker mutex poisoned")
            .clone();
        let tmp_path = path.with_extension("jsonl.tmp");
        {
            let mut tmp = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            for line in keep {
                writeln!(tmp, "{line}")?;
            }
        }
        fs::rename(&tmp_path, &path)?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tracker(label: &str) -> (TaskTracker, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "sirin_persona_test_{}_{}.jsonl",
            std::process::id(),
            label
        ));
        (TaskTracker::new(&path), path)
    }

    #[test]
    fn tracker_record_and_read_roundtrip() {
        let (tracker, path) = tmp_tracker("roundtrip");
        let entry = TaskEntry::heartbeat("TestPersona");
        tracker.record(&entry).expect("record should succeed");

        let entries = tracker.read_last_n(10).expect("read should succeed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].persona, "TestPersona");
        assert_eq!(entries[0].event, "heartbeat");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_read_last_n_returns_tail() {
        let (tracker, path) = tmp_tracker("tail");
        for i in 0..10usize {
            let mut e = TaskEntry::heartbeat("P");
            e.reason = Some(format!("entry {i}"));
            tracker.record(&e).expect("record ok");
        }
        let entries = tracker.read_last_n(3).expect("read ok");
        assert_eq!(entries.len(), 3);
        // The 3 newest entries should be entries 7, 8, 9.
        assert_eq!(entries[2].reason.as_deref(), Some("entry 9"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_read_missing_file_returns_empty() {
        let path = std::env::temp_dir().join("sirin_nonexistent_tracker.jsonl");
        let _ = std::fs::remove_file(&path); // ensure absent
        let tracker = TaskTracker::new(&path);
        let entries = tracker
            .read_last_n(10)
            .expect("should succeed even if file is absent");
        assert!(entries.is_empty());
    }

    #[test]
    fn tracker_update_statuses_rewrites_atomically() {
        let (tracker, path) = tmp_tracker("update");
        let mut entry = TaskEntry::heartbeat("P");
        entry.status = Some("PENDING".to_string());
        let ts = entry.timestamp.clone();
        tracker.record(&entry).expect("record ok");

        let mut updates = std::collections::HashMap::new();
        updates.insert(ts, "DONE".to_string());
        tracker.update_statuses(&updates).expect("update ok");

        let entries = tracker.read_last_n(10).expect("read ok");
        assert_eq!(entries[0].status.as_deref(), Some("DONE"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_update_statuses_noop_when_empty() {
        let (tracker, path) = tmp_tracker("noop");
        tracker
            .update_statuses(&std::collections::HashMap::new())
            .expect("noop ok");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_trim_to_max_removes_oldest_entries() {
        let (tracker, path) = tmp_tracker("trim");
        for i in 0..8usize {
            let mut e = TaskEntry::heartbeat("P");
            e.reason = Some(format!("entry {i}"));
            tracker.record(&e).expect("record ok");
        }
        let removed = tracker.trim_to_max(5).expect("trim ok");
        assert_eq!(removed, 3);
        let remaining = tracker.read_last_n(10).expect("read ok");
        assert_eq!(remaining.len(), 5);
        assert_eq!(
            remaining[0].reason.as_deref(),
            Some("entry 3"),
            "oldest kept should be entry 3"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_trim_to_max_noop_when_under_limit() {
        let (tracker, path) = tmp_tracker("trim_noop");
        for _ in 0..3 {
            tracker
                .record(&TaskEntry::heartbeat("P"))
                .expect("record ok");
        }
        let removed = tracker.trim_to_max(10).expect("trim ok");
        assert_eq!(removed, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_find_by_timestamp_returns_correct_entry() {
        let (tracker, path) = tmp_tracker("find");
        let mut entry = TaskEntry::heartbeat("P");
        entry.reason = Some("unique-reason".to_string());
        let ts = entry.timestamp.clone();
        tracker.record(&entry).expect("record ok");

        let found = tracker.find_by_timestamp(&ts).expect("find ok");
        assert!(found.is_some());
        assert_eq!(found.unwrap().reason.as_deref(), Some("unique-reason"));
        std::fs::remove_file(&path).ok();
    }
}
