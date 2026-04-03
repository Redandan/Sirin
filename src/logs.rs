/// Structured Logging System
///
/// Provides in-memory log storage and retrieval for real-time frontend consumption.
/// Logs are stored in a circular buffer to bound memory usage.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const MAX_LOGS: usize = 5000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub target: String,
    pub message: String,
    pub context: Option<String>,
}

pub struct LogStore {
    entries: Arc<Mutex<Vec<LogEntry>>>,
}

impl LogStore {
    pub fn new() -> Self {
        LogStore {
            entries: Arc::new(Mutex::new(Vec::with_capacity(MAX_LOGS))),
        }
    }

    pub fn push(&self, entry: LogEntry) {
        let mut entries = self.entries.lock();
        entries.push(entry);
        
        // Keep circular buffer bounded
        if entries.len() > MAX_LOGS {
            entries.remove(0);
        }
    }

    pub fn get_all(&self) -> Vec<LogEntry> {
        self.entries.lock().clone()
    }

    pub fn get_recent(&self, limit: usize) -> Vec<LogEntry> {
        let entries = self.entries.lock();
        let start = if entries.len() > limit {
            entries.len() - limit
        } else {
            0
        };
        entries[start..].to_vec()
    }

    pub fn filter(&self, target: Option<&str>, level: Option<&str>) -> Vec<LogEntry> {
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|e| {
                let target_match = target.map_or(true, |t| e.target.contains(t));
                let level_match = level.map_or(true, |l| e.level == l);
                target_match && level_match
            })
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
    }
}

impl Clone for LogStore {
    fn clone(&self) -> Self {
        LogStore {
            entries: Arc::clone(&self.entries),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_store() {
        let store = LogStore::new();
        let entry = LogEntry {
            timestamp: Utc::now(),
            level: "INFO".to_string(),
            target: "test".to_string(),
            message: "test message".to_string(),
            context: None,
        };

        store.push(entry.clone());
        let all = store.get_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].message, "test message");
    }

    #[test]
    fn test_max_logs_bounded() {
        let store = LogStore::new();
        for i in 0..6000 {
            let entry = LogEntry {
                timestamp: Utc::now(),
                level: "INFO".to_string(),
                target: "test".to_string(),
                message: format!("message {}", i),
                context: None,
            };
            store.push(entry);
        }

        let all = store.get_all();
        assert_eq!(all.len(), MAX_LOGS);
    }
}
