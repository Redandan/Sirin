//! Persistent chat history for the Workspace 對話 tab.
//!
//! Backed by the shared SQLite at `<app_data_dir>/memory/test_memory.db`
//! (table `chat_messages` — see `test_runner::store`). One row per message,
//! ordered by `created_at`.
//!
//! AI-friendly notes:
//!   • Pure functions — no shared state, no globals beyond the DB Mutex.
//!   • Both `append` and `history` lock the same mutex briefly; appending
//!     during a poll is safe.
//!   • `text` is stored verbatim (no length cap). Future PR can add a
//!     `purge_older_than(days)` for retention if the table grows.

use rusqlite::params;

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    /// "user" | "agent"
    pub role:       String,
    pub text:       String,
    /// RFC-3339 UTC timestamp.
    pub created_at: String,
}

fn db() -> &'static std::sync::Mutex<rusqlite::Connection> {
    crate::test_runner::store::__shared_db()
}

/// Append one message. Caller is expected to invoke this twice per chat
/// turn — once for the user message, once for the agent reply.
pub fn append(agent_id: &str, role: &str, text: &str) -> Result<(), String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO chat_messages (agent_id, role, text, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![agent_id, role, text, chrono::Utc::now().to_rfc3339()],
    ).map_err(|e| format!("insert chat_messages: {e}"))?;
    Ok(())
}

/// Most recent `limit` messages for an agent, oldest first (so the UI can
/// `for…of` directly). Pass a generous limit (e.g. 200); the UI virtualizes.
pub fn history(agent_id: &str, limit: usize) -> Result<Vec<ChatMessage>, String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    let mut stmt = conn.prepare(
        "SELECT role, text, created_at FROM chat_messages \
         WHERE agent_id = ?1 \
         ORDER BY id DESC LIMIT ?2",
    ).map_err(|e| format!("prepare history: {e}"))?;
    let rows = stmt.query_map(params![agent_id, limit as i64], |r| {
        Ok(ChatMessage {
            role:       r.get(0)?,
            text:       r.get(1)?,
            created_at: r.get(2)?,
        })
    }).map_err(|e| format!("query history: {e}"))?;
    let mut out: Vec<ChatMessage> = rows.collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    // Reverse so oldest is first (we queried DESC for the LIMIT).
    out.reverse();
    Ok(out)
}

/// Wipe an agent's history. Useful for "重置對話".
#[allow(dead_code)]
pub fn clear(agent_id: &str) -> Result<(), String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM chat_messages WHERE agent_id = ?1",
        params![agent_id],
    ).map_err(|e| format!("clear: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_history_roundtrip() {
        let _ = clear("test_a");
        append("test_a", "user",  "hello").unwrap();
        append("test_a", "agent", "hi there").unwrap();
        let h = history("test_a", 100).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, "user");
        assert_eq!(h[0].text, "hello");
        assert_eq!(h[1].role, "agent");
        assert_eq!(h[1].text, "hi there");
    }

    #[test]
    fn history_isolated_per_agent() {
        let _ = clear("iso_a");
        let _ = clear("iso_b");
        append("iso_a", "user", "for A").unwrap();
        append("iso_b", "user", "for B").unwrap();
        let a = history("iso_a", 100).unwrap();
        let b = history("iso_b", 100).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].text, "for A");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "for B");
    }
}
