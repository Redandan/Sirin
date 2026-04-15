//! Per-peer conversation context — a JSONL ring-log of user↔assistant turns.
//!
//! One file per (agent_id, peer_id) combo under `{app_data}/tracking/`.
//! Used by Chat/Coding agents to inject recent dialogue into the LLM prompt,
//! and by persona-learning to sample past replies.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};

fn context_log_path(peer_id: Option<i64>, agent_id: Option<&str>) -> PathBuf {
    let filename = match (agent_id, peer_id) {
        (Some(aid), Some(pid)) => format!("sirin_context_{aid}_{pid}.jsonl"),
        (Some(aid), None) => format!("sirin_context_{aid}.jsonl"),
        (None, Some(pid)) => format!("sirin_context_{pid}.jsonl"),
        (None, None) => "sirin_context.jsonl".to_string(),
    };
    crate::platform::app_data_dir().join("tracking").join(&filename)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEntry {
    pub timestamp: String,
    pub user_msg: String,
    pub assistant_reply: String,
}

/// Append a conversation turn to the per-peer context log.
pub fn append_context(
    user_msg: &str,
    assistant_reply: &str,
    peer_id: Option<i64>,
    agent_id: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = context_log_path(peer_id, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = ContextEntry {
        timestamp: Utc::now().to_rfc3339(),
        user_msg: user_msg.to_string(),
        assistant_reply: assistant_reply.to_string(),
    };
    let line = serde_json::to_string(&entry)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Load the most recent `limit` context entries for a specific peer.
pub fn load_recent_context(
    limit: usize,
    peer_id: Option<i64>,
    agent_id: Option<&str>,
) -> Result<Vec<ContextEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let path = context_log_path(peer_id, agent_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);

    let mut ring: VecDeque<ContextEntry> = VecDeque::with_capacity(limit);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<ContextEntry>(&line) {
            if ring.len() == limit {
                ring.pop_front();
            }
            ring.push_back(entry);
        }
    }
    Ok(ring.into_iter().collect())
}

/// Collect the most-recent `limit` assistant replies across ALL context files
/// belonging to the given agent.  Used for persona-learning analysis.
pub fn collect_reply_samples(agent_id: &str, limit: usize) -> Vec<String> {
    let dir = crate::platform::app_data_dir().join("tracking");
    let prefix = format!("sirin_context_{agent_id}");
    let mut samples: Vec<(String, String)> = Vec::new(); // (timestamp, reply)

    let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".jsonl") { continue; }
        let Ok(file) = fs::File::open(entry.path()) else { continue };
        for line in BufReader::new(file).lines().filter_map(|l| l.ok()) {
            if let Ok(ctx) = serde_json::from_str::<ContextEntry>(&line) {
                if !ctx.assistant_reply.trim().is_empty() {
                    samples.push((ctx.timestamp, ctx.assistant_reply));
                }
            }
        }
    }
    samples.sort_by(|a, b| b.0.cmp(&a.0));
    samples.into_iter().take(limit).map(|(_, r)| r).collect()
}
