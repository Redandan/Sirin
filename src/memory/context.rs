//! Per-peer conversation context — a JSONL ring-log of user↔assistant turns.
//!
//! One file per (agent_id, peer_id) combo under `{app_data}/tracking/`.
//! Used by Chat/Coding agents to inject recent dialogue into the LLM prompt,
//! and by persona-learning to sample past replies.
//!
//! Concurrency: each per-peer file is an independent [`JsonlLog`]; the
//! read/append operations are serialised by that log's internal mutex but do
//! not block across different peers.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::jsonl_log::JsonlLog;

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
    let entry = ContextEntry {
        timestamp: Utc::now().to_rfc3339(),
        user_msg: user_msg.to_string(),
        assistant_reply: assistant_reply.to_string(),
    };
    JsonlLog::<ContextEntry>::new(context_log_path(peer_id, agent_id))
        .append(&entry)
        .map_err(Into::into)
}

/// Load the most recent `limit` context entries for a specific peer.
pub fn load_recent_context(
    limit: usize,
    peer_id: Option<i64>,
    agent_id: Option<&str>,
) -> Result<Vec<ContextEntry>, Box<dyn std::error::Error + Send + Sync>> {
    JsonlLog::<ContextEntry>::new(context_log_path(peer_id, agent_id))
        .read_last_n(limit)
        .map_err(Into::into)
}

/// Collect the most-recent `limit` assistant replies across ALL context files
/// belonging to the given agent.  Used for persona-learning analysis.
///
/// Scans the tracking directory by filename prefix rather than going through
/// JsonlLog because the files are owned by many different `(peer_id)` keys
/// that we don't enumerate elsewhere.
pub fn collect_reply_samples(agent_id: &str, limit: usize) -> Vec<String> {
    let dir = crate::platform::app_data_dir().join("tracking");
    let prefix = format!("sirin_context_{agent_id}");
    let mut samples: Vec<(String, String)> = Vec::new(); // (timestamp, reply)

    let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".jsonl") {
            continue;
        }
        let Ok(file) = fs::File::open(entry.path()) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
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
