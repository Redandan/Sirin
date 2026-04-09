//! Persistent store for AI-drafted replies that require human approval before
//! being sent (the "Human-in-the-loop" confirmation flow).
//!
//! Replies are stored as newline-delimited JSON (JSONL) at
//! `data/pending_replies/{agent_id}.jsonl`.  Each record is a
//! [`PendingReply`].  The full list is re-written on every update (the file
//! is expected to stay small — operator reviews replies promptly).

use std::{
    fs,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

// ── Types ─────────────────────────────────────────────────────────────────────

/// Lifecycle state of one pending reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingStatus {
    /// Waiting for human review.
    Pending,
    /// Operator approved — message was (or will be) sent.
    Approved,
    /// Operator rejected — message will not be sent.
    Rejected,
}

/// One AI-drafted reply waiting for operator confirmation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingReply {
    /// Unique ID: `"{unix_timestamp_ms}_{agent_id}"`.
    pub id: String,
    /// Which agent produced this draft.
    pub agent_id: String,
    /// Platform string: `"telegram"` or `"teams"`.
    pub platform: String,
    /// Telegram peer ID (chat_id) — `None` for Teams or UI-only agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<i64>,
    /// Teams conversation ID (data-convid) — `None` for Telegram agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Human-readable sender / conversation name.
    pub peer_name: String,
    /// The original incoming message that triggered this draft.
    pub original_message: String,
    /// The AI-generated reply draft (editable by the operator).
    pub draft_reply: String,
    /// Optional research notes produced alongside the draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_notes: Option<String>,
    /// ISO 8601 timestamp when the draft was created.
    pub created_at: String,
    /// Current lifecycle state.
    pub status: PendingStatus,
}

impl PendingReply {
    /// Construct a new pending reply with a generated ID and `Pending` status.
    pub fn new(
        agent_id: impl Into<String>,
        platform: impl Into<String>,
        peer_id: Option<i64>,
        peer_name: impl Into<String>,
        original_message: impl Into<String>,
        draft_reply: impl Into<String>,
    ) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let agent_id = agent_id.into();
        let id = format!("{now_ms}_{agent_id}");
        let created_at = chrono::Utc::now().to_rfc3339();
        Self {
            id,
            agent_id,
            platform: platform.into(),
            peer_id,
            chat_id: None,
            peer_name: peer_name.into(),
            original_message: original_message.into(),
            draft_reply: draft_reply.into(),
            research_notes: None,
            created_at,
            status: PendingStatus::Pending,
        }
    }
}

// ── Persistence helpers ───────────────────────────────────────────────────────

/// Path to the JSONL file for a given agent.
pub fn pending_replies_path(agent_id: &str) -> PathBuf {
    PathBuf::from("data").join("pending_replies").join(format!("{agent_id}.jsonl"))
}

/// Load all pending replies for an agent.  Returns an empty Vec if the file
/// does not exist or cannot be parsed.
pub fn load_pending(agent_id: &str) -> Vec<PendingReply> {
    let path = pending_replies_path(agent_id);
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Persist the full list of replies for an agent (overwrites existing file).
pub fn save_pending(
    agent_id: &str,
    replies: &[PendingReply],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = pending_replies_path(agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for r in replies {
        out.push_str(&serde_json::to_string(r)?);
        out.push('\n');
    }
    fs::write(&path, &out)?;
    Ok(())
}

/// Append one new reply to the store (load → push → save).
pub fn append_pending(reply: PendingReply) {
    let agent_id = reply.agent_id.clone();
    let mut replies = load_pending(&agent_id);
    replies.push(reply);
    let _ = save_pending(&agent_id, &replies);
}

/// Update the status of one reply by ID.  No-op if the ID is not found.
pub fn update_status(agent_id: &str, id: &str, status: PendingStatus) {
    let mut replies = load_pending(agent_id);
    if let Some(r) = replies.iter_mut().find(|r| r.id == id) {
        r.status = status;
    }
    let _ = save_pending(agent_id, &replies);
}

/// Delete a reply by ID.
pub fn delete_pending(agent_id: &str, id: &str) {
    let mut replies = load_pending(agent_id);
    replies.retain(|r| r.id != id);
    let _ = save_pending(agent_id, &replies);
}
