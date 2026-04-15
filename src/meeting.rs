//! Internal meeting room — operator-driven multi-agent session with
//! runtime handoff authorisation.
//!
//! ## Concurrency
//! Single active session lives in a `RwLock<Option<MeetingSession>>`.
//! `current_meeting_id` / `readable_owners` / `check_meeting_auth` take the
//! read lock; `start_meeting` / `end_meeting` / `append_turn` take the write
//! lock briefly.  Parallel `start_meeting` calls would replace each other —
//! operator UI is expected to coordinate so only one caller starts at a time.
//!
//! # Usage
//! ```
//! meeting::start_meeting(vec!["assistant_1".into(), "assistant_2".into()]);
//! meeting::grant_auth("assistant_1", "assistant_2", meeting::AuthScope::SessionOnly);
//! // … operator or AI relays messages …
//! meeting::end_meeting();
//! ```

use std::sync::{Mutex, OnceLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AuthScope {
    /// Only valid for the current meeting session.
    SessionOnly,
    /// Also writes to `trusted_senders` config (persistent).
    Permanent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingAuth {
    pub from_agent: String,
    pub to_agent:   String,
    pub scope:      AuthScope,
}

/// Grants `reader_agent_id` read access to `owner_agent_id`'s private memories
/// for the duration of this meeting session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryShare {
    pub reader_agent_id: String,
    pub owner_agent_id:  String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RequestStatus {
    Pending,
    Approved,
    Denied,
}

/// A runtime request from an agent to read another agent's private memories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryAccessRequest {
    pub id:           String,
    pub requester_id: String,
    pub owner_id:     String,
    /// Short hint from the LLM about what it was looking for.
    pub query_hint:   String,
    pub status:       RequestStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingTurn {
    pub speaker:   String,
    pub text:      String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingSession {
    pub id:              String,
    pub participants:    Vec<String>,
    pub auths:           Vec<MeetingAuth>,
    pub memory_shares:   Vec<MemoryShare>,
    pub access_requests: Vec<MemoryAccessRequest>,
    pub turns:           Vec<MeetingTurn>,
    pub started_at:      String,
}

// ── Singleton ─────────────────────────────────────────────────────────────────

static ACTIVE_MEETING: OnceLock<Mutex<Option<MeetingSession>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<MeetingSession>> {
    ACTIVE_MEETING.get_or_init(|| Mutex::new(None))
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Start a new meeting session with the given participant agent IDs.
/// Returns the new session ID.
pub fn start_meeting(participants: Vec<String>) -> String {
    let id = format!("mtg-{}", Utc::now().timestamp());
    *state().lock().unwrap_or_else(|e| e.into_inner()) = Some(MeetingSession {
        id: id.clone(),
        participants,
        auths:           vec![],
        memory_shares:   vec![],
        access_requests: vec![],
        turns:           vec![],
        started_at:      Utc::now().to_rfc3339(),
    });
    id
}

/// End the current meeting, clearing all session auth grants.
pub fn end_meeting() {
    *state().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Grant `from` the ability to call `confidential_handoff` targeting `to`
/// for the duration of this meeting (or permanently if `scope == Permanent`).
pub fn grant_auth(from: &str, to: &str, scope: AuthScope) {
    if let Some(s) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        // Avoid duplicates
        if !s.auths.iter().any(|a| a.from_agent == from && a.to_agent == to) {
            s.auths.push(MeetingAuth {
                from_agent: from.to_string(),
                to_agent:   to.to_string(),
                scope,
            });
        }
    }
}

/// Revoke a previously granted auth pair.
pub fn revoke_auth(from: &str, to: &str) {
    if let Some(s) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        s.auths.retain(|a| !(a.from_agent == from && a.to_agent == to));
    }
}

/// Append a spoken turn to the session transcript.
pub fn append_turn(speaker: &str, text: &str) {
    if let Some(s) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        s.turns.push(MeetingTurn {
            speaker:   speaker.to_string(),
            text:      text.to_string(),
            timestamp: Utc::now().to_rfc3339(),
        });
    }
}

/// Get all turns from the current meeting as (speaker, text) pairs.
pub fn get_turns() -> Vec<(String, String)> {
    state().lock().unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|s| s.turns.iter().map(|t| (t.speaker.clone(), t.text.clone())).collect())
        .unwrap_or_default()
}

/// Returns `true` if `from` currently has meeting-level auth to hand off to `to`.
pub fn check_meeting_auth(from: &str, to: &str) -> bool {
    state()
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.auths.iter().any(|a| a.from_agent == from && a.to_agent == to))
        .unwrap_or(false)
}

// ── Memory-share API ──────────────────────────────────────────────────────────

/// Grant `reader` read access to `owner`'s private (`agent_memories`) records
/// for the lifetime of this session.
pub fn grant_memory_share(reader: &str, owner: &str) {
    if let Some(s) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        if !s.memory_shares.iter().any(|ms| ms.reader_agent_id == reader && ms.owner_agent_id == owner) {
            s.memory_shares.push(MemoryShare {
                reader_agent_id: reader.to_string(),
                owner_agent_id:  owner.to_string(),
            });
        }
    }
}

/// Revoke a previously granted memory-share.
pub fn revoke_memory_share(reader: &str, owner: &str) {
    if let Some(s) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        s.memory_shares.retain(|ms| !(ms.reader_agent_id == reader && ms.owner_agent_id == owner));
    }
}

/// Returns `true` if `reader` currently has session-level read access to
/// `owner`'s private memories.
pub fn can_read_memory(reader: &str, owner: &str) -> bool {
    state()
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.memory_shares.iter().any(|ms| ms.reader_agent_id == reader && ms.owner_agent_id == owner))
        .unwrap_or(false)
}

/// Returns all agent IDs whose private memories `reader` is allowed to search.
pub fn readable_owners(reader: &str) -> Vec<String> {
    state()
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| {
            s.memory_shares
                .iter()
                .filter(|ms| ms.reader_agent_id == reader)
                .map(|ms| ms.owner_agent_id.clone())
                .collect()
        })
        .unwrap_or_default()
}

// ── Memory-access request API ─────────────────────────────────────────────────

/// Record a pending request from `requester` to read `owner`'s memories.
/// Returns the new request ID.  No-op if an identical Pending request already
/// exists (deduplication by requester+owner).
pub fn request_memory_access(requester: &str, owner: &str, hint: &str) -> String {
    let id = format!("req-{}", Utc::now().timestamp_millis());
    if let Some(s) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let already = s.access_requests.iter().any(|r| {
            r.requester_id == requester
                && r.owner_id == owner
                && r.status == RequestStatus::Pending
        });
        if !already {
            s.access_requests.push(MemoryAccessRequest {
                id: id.clone(),
                requester_id: requester.to_string(),
                owner_id:     owner.to_string(),
                query_hint:   hint.to_string(),
                status:       RequestStatus::Pending,
            });
        }
    }
    id
}

/// Approve or deny a pending memory-access request by ID.
/// Approving automatically calls `grant_memory_share`.
pub fn resolve_access_request(request_id: &str, approved: bool) {
    let mut share: Option<(String, String)> = None;
    if let Some(s) = state().lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        if let Some(req) = s.access_requests.iter_mut().find(|r| r.id == request_id) {
            req.status = if approved { RequestStatus::Approved } else { RequestStatus::Denied };
            if approved {
                share = Some((req.requester_id.clone(), req.owner_id.clone()));
            }
        }
    }
    if let Some((reader, owner)) = share {
        grant_memory_share(&reader, &owner);
    }
}

/// Returns all currently Pending memory-access requests.
pub fn pending_requests() -> Vec<MemoryAccessRequest> {
    state()
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| {
            s.access_requests
                .iter()
                .filter(|r| r.status == RequestStatus::Pending)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Read the active session without keeping the lock across await boundaries.
/// Clones are cheap for small sessions.
pub fn with_session<T>(f: impl FnOnce(Option<&MeetingSession>) -> T) -> T {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    f(guard.as_ref())
}

/// Returns the current meeting ID, or `None` if no meeting is active.
pub fn current_meeting_id() -> Option<String> {
    state().lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.id.clone())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize meeting tests because they share the global singleton.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn start_creates_session_with_participants() {
        let _g = TEST_LOCK.lock().unwrap();
        end_meeting();
        let id = start_meeting(vec!["a1".into(), "a2".into()]);
        assert!(id.starts_with("mtg-"));
        assert_eq!(current_meeting_id(), Some(id));
        with_session(|s| {
            let s = s.unwrap();
            assert_eq!(s.participants, vec!["a1", "a2"]);
        });
        end_meeting();
    }

    #[test]
    fn end_clears_session() {
        let _g = TEST_LOCK.lock().unwrap();
        start_meeting(vec!["a1".into()]);
        assert!(current_meeting_id().is_some());
        end_meeting();
        assert!(current_meeting_id().is_none());
    }

    #[test]
    fn grant_and_check_auth() {
        let _g = TEST_LOCK.lock().unwrap();
        end_meeting();
        start_meeting(vec!["a1".into(), "a2".into()]);
        assert!(!check_meeting_auth("a1", "a2"), "no auth before grant");
        grant_auth("a1", "a2", AuthScope::SessionOnly);
        assert!(check_meeting_auth("a1", "a2"), "auth should be active after grant");
        assert!(!check_meeting_auth("a2", "a1"), "reverse direction must remain unauthorized");
        end_meeting();
    }

    #[test]
    fn revoke_removes_auth() {
        let _g = TEST_LOCK.lock().unwrap();
        end_meeting();
        start_meeting(vec!["a1".into(), "a2".into()]);
        grant_auth("a1", "a2", AuthScope::SessionOnly);
        assert!(check_meeting_auth("a1", "a2"));
        revoke_auth("a1", "a2");
        assert!(!check_meeting_auth("a1", "a2"), "auth must be gone after revoke");
        end_meeting();
    }

    #[test]
    fn grant_deduplicates() {
        let _g = TEST_LOCK.lock().unwrap();
        end_meeting();
        start_meeting(vec!["a1".into(), "a2".into()]);
        grant_auth("a1", "a2", AuthScope::SessionOnly);
        grant_auth("a1", "a2", AuthScope::SessionOnly); // duplicate
        let count = with_session(|s| {
            s.map(|sess| sess.auths.len()).unwrap_or(0)
        });
        assert_eq!(count, 1, "duplicate grant should not add a second entry");
        end_meeting();
    }

    #[test]
    fn no_auth_without_active_meeting() {
        let _g = TEST_LOCK.lock().unwrap();
        end_meeting();
        assert!(!check_meeting_auth("a1", "a2"));
    }

    #[test]
    fn append_turn_records_in_history() {
        let _g = TEST_LOCK.lock().unwrap();
        end_meeting();
        start_meeting(vec!["a1".into()]);
        append_turn("a1", "hello from a1");
        let turns = with_session(|s| {
            s.map(|sess| sess.turns.clone()).unwrap_or_default()
        });
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].speaker, "a1");
        assert_eq!(turns[0].text, "hello from a1");
        end_meeting();
    }

    #[test]
    fn end_meeting_clears_auths() {
        let _g = TEST_LOCK.lock().unwrap();
        start_meeting(vec!["a1".into(), "a2".into()]);
        grant_auth("a1", "a2", AuthScope::SessionOnly);
        end_meeting();
        // After ending, auth must be gone even if we somehow start a new meeting.
        start_meeting(vec!["a1".into(), "a2".into()]);
        assert!(!check_meeting_auth("a1", "a2"), "new meeting starts with no auths");
        end_meeting();
    }
}
