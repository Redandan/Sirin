//! Persistent memory for Sirin.
//!
//! ## Concurrency
//! - The SQLite connection is wrapped in a process-wide `Mutex` — every
//!   `memory_store` / `memory_search` / `memory_list_recent` call serialises
//!   through it.  FTS5 search blocks concurrent writes (and vice versa).
//! - The per-peer context and codebase-index files are each their own
//!   [`crate::jsonl_log::JsonlLog`] instance and do not contend with the SQL
//!   connection.
//!
//! ## Three layers
//!
//! 1. **Full-text memory store** (`memory_store` / `memory_search`)
//!    SQLite FTS5 database at `{app_data}/memory/memories.db`.
//!    This module (`mod.rs`) hosts the SQL store.
//!
//! 2. **Project codebase index** ([`codebase`] submodule)
//!    Periodically scans the local repository and stores architecture-aware
//!    file summaries; TF-scored keyword search across the index.
//!
//! 3. **Conversation context** ([`context`] submodule)
//!    Per-peer JSONL ring-log of recent user↔assistant turns.

mod codebase;
mod context;

pub use codebase::{
    ensure_codebase_index, inspect_project_file_range, list_project_files, looks_like_code_query,
    refresh_codebase_index, search_codebase,
};
pub use context::{append_context, load_recent_context};

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ── Memory store (SQLite FTS5 backend) ───────────────────────────────────────

fn memory_db_path() -> PathBuf {
    crate::platform::app_data_dir().join("memory").join("memories.db")
}

/// Legacy JSONL path — used only for one-time migration on first startup.
fn memory_index_path() -> PathBuf {
    crate::platform::app_data_dir().join("memory").join("index.jsonl")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryEntry {
    timestamp: String,
    source: String,
    text: String,
}

/// Return the process-wide SQLite connection (initialised once).
///
/// On first call, creates the FTS5 schema and migrates any existing JSONL data.
fn memory_db() -> &'static Mutex<rusqlite::Connection> {
    static DB: OnceLock<Mutex<rusqlite::Connection>> = OnceLock::new();
    DB.get_or_init(|| {
        let path = memory_db_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn =
            rusqlite::Connection::open(&path).expect("Failed to open memory SQLite database");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts \
             USING fts5(text, source, timestamp, tokenize='unicode61'); \
             CREATE TABLE IF NOT EXISTS agent_memories ( \
                 id             INTEGER PRIMARY KEY AUTOINCREMENT, \
                 text           TEXT    NOT NULL, \
                 source         TEXT    NOT NULL, \
                 timestamp      TEXT    NOT NULL, \
                 owner_agent_id TEXT    NOT NULL \
             ); \
             CREATE INDEX IF NOT EXISTS idx_am_owner \
                 ON agent_memories(owner_agent_id);",
        )
        .expect("Failed to initialize memory schema");

        // One-time migration from legacy JSONL on startup.
        migrate_jsonl_to_sqlite(&conn);

        Mutex::new(conn)
    })
}

/// Import any entries from the legacy JSONL file that don't yet exist in SQLite.
/// Safe to call repeatedly — skips migration when the FTS5 table is non-empty.
fn migrate_jsonl_to_sqlite(conn: &rusqlite::Connection) {
    let jsonl_path = memory_index_path();
    if !jsonl_path.exists() {
        return;
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories_fts", [], |r| r.get(0))
        .unwrap_or(0);
    if count > 0 {
        return; // Already migrated.
    }
    let file = match fs::File::open(&jsonl_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut stmt = match conn
        .prepare("INSERT INTO memories_fts(text, source, timestamp) VALUES (?1, ?2, ?3)")
    {
        Ok(s) => s,
        Err(_) => return,
    };
    let migrated = BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<MemoryEntry>(&l).ok())
        .filter_map(|e| {
            stmt.execute(rusqlite::params![e.text, e.source, e.timestamp])
                .ok()
        })
        .count();
    if migrated > 0 {
        eprintln!("[memory] migrated {migrated} JSONL entries → SQLite FTS5");
    }
}

/// Persist a text snippet to the memory store.
///
/// - `owner_agent_id`: the agent that owns this entry.  Pass `""` for global
///   shared memory (written to `memories_fts`, existing behaviour).
/// - `visibility`: `"shared"` | `"confidential"`.  When `"confidential"` **and**
///   `owner_agent_id` is non-empty the entry is written to `agent_memories` and
///   is only readable by that agent.  All other combinations go to `memories_fts`.
pub fn memory_store(
    text: &str,
    source: &str,
    owner_agent_id: &str,
    visibility: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let timestamp = Utc::now().to_rfc3339();
    let conn = memory_db()
        .lock()
        .map_err(|e| format!("memory DB lock poisoned: {e}"))?;
    if !owner_agent_id.is_empty() && visibility == "confidential" {
        conn.execute(
            "INSERT INTO agent_memories(text, source, timestamp, owner_agent_id) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![text, source, timestamp, owner_agent_id],
        )?;
    } else {
        conn.execute(
            "INSERT INTO memories_fts(text, source, timestamp) VALUES (?1, ?2, ?3)",
            rusqlite::params![text, source, timestamp],
        )?;
    }
    Ok(())
}

/// Return the N most-recently stored memory entries (no query required).
///
/// When `caller_agent_id` is non-empty the result also includes the most recent
/// entries from `agent_memories` owned by that agent.
pub fn memory_list_recent(
    limit: usize,
    caller_agent_id: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let conn = memory_db()
        .lock()
        .map_err(|e| format!("memory DB lock poisoned: {e}"))?;
    let mut stmt = conn.prepare(
        "SELECT text FROM memories_fts ORDER BY rowid DESC LIMIT ?1",
    )?;
    let mut results: Vec<String> = stmt
        .query_map(rusqlite::params![limit as i64], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    if !caller_agent_id.is_empty() {
        let mut stmt2 = conn.prepare(
            "SELECT text FROM agent_memories \
             WHERE owner_agent_id = ?1 \
             ORDER BY id DESC LIMIT ?2",
        )?;
        let agent_results: Vec<String> = stmt2
            .query_map(rusqlite::params![caller_agent_id, limit as i64], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        results.extend(agent_results);
        results.truncate(limit);
    }
    Ok(results)
}

/// Full-text search the memory store using SQLite FTS5.
///
/// Results are ranked by FTS5 relevance (BM25) and capped at `limit`.
///
/// When `caller_agent_id` is non-empty the result also includes entries from
/// `agent_memories` owned by that agent (LIKE search).  Pass `""` for anonymous
/// callers (e.g. MCP / RPC) — they only see shared memories.
pub fn memory_search(
    query: &str,
    limit: usize,
    caller_agent_id: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let safe_query = sanitize_fts5_query(query);
    let conn = memory_db()
        .lock()
        .map_err(|e| format!("memory DB lock poisoned: {e}"))?;
    let mut stmt = conn.prepare(
        "SELECT text FROM memories_fts \
         WHERE memories_fts MATCH ?1 \
         ORDER BY rank \
         LIMIT ?2",
    )?;
    let mut results: Vec<String> = stmt
        .query_map(rusqlite::params![safe_query, limit as i64], |row| {
            row.get(0)
        })?
        .filter_map(|r| r.ok())
        .collect();
    if !caller_agent_id.is_empty() {
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        // Own private memories.
        let mut stmt2 = conn.prepare(
            "SELECT text FROM agent_memories \
             WHERE owner_agent_id = ?1 AND text LIKE ?2 ESCAPE '\\' \
             LIMIT ?3",
        )?;
        let agent_results: Vec<String> = stmt2
            .query_map(
                rusqlite::params![caller_agent_id, pattern, limit as i64],
                |row| row.get(0),
            )?
            .filter_map(|r| r.ok())
            .collect();
        results.extend(agent_results);

        // Memories shared with this caller by other agents in the active meeting.
        let shared_owners = crate::meeting::readable_owners(caller_agent_id);
        for owner in &shared_owners {
            let mut stmt3 = conn.prepare(
                "SELECT text FROM agent_memories \
                 WHERE owner_agent_id = ?1 AND text LIKE ?2 ESCAPE '\\' \
                 LIMIT ?3",
            )?;
            let shared: Vec<String> = stmt3
                .query_map(
                    rusqlite::params![owner, pattern, limit as i64],
                    |row| row.get(0),
                )?
                .filter_map(|r| r.ok())
                .collect();
            results.extend(shared);
        }

        results.truncate(limit);
    }
    Ok(results)
}

/// Sanitize a user query string for use in an FTS5 MATCH expression.
///
/// Wraps each whitespace-separated token in double quotes so that special FTS5
/// syntax characters (parentheses, operators, etc.) can't cause parse errors.
fn sanitize_fts5_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|w| {
            let safe: String = w.chars().filter(|&c| c != '"').collect();
            format!("\"{safe}\"")
        })
        .collect();
    if tokens.is_empty() {
        return "\"\"".to_string();
    }
    tokens.join(" ")
}

// ── Tests — agent_memories isolation ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Confidential memory is NOT visible to anonymous callers (MCP / RPC path).
    #[test]
    fn confidential_hidden_from_anonymous() {
        let unique = format!("confidential_test_anon_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
        memory_store(&unique, "test", "agent_isolation_a", "confidential")
            .expect("store should succeed");

        let found = memory_search(&unique, 5, "")
            .expect("search should succeed");
        assert!(
            !found.iter().any(|r| r.contains(&unique)),
            "anonymous search must NOT see confidential memory: {:?}", found
        );
    }

    /// Owner agent can retrieve its own confidential memory.
    #[test]
    fn owner_can_read_confidential() {
        let unique = format!("confidential_test_owner_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
        memory_store(&unique, "test", "agent_isolation_b", "confidential")
            .expect("store should succeed");

        let found = memory_search(&unique, 5, "agent_isolation_b")
            .expect("search should succeed");
        assert!(
            found.iter().any(|r| r.contains(&unique)),
            "owner must see its own confidential memory: {:?}", found
        );
    }

    /// A different agent cannot read another agent's confidential memory.
    #[test]
    fn other_agent_cannot_read_confidential() {
        let unique = format!("confidential_test_other_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
        memory_store(&unique, "test", "agent_isolation_c", "confidential")
            .expect("store should succeed");

        let found = memory_search(&unique, 5, "agent_isolation_d")
            .expect("search should succeed");
        assert!(
            !found.iter().any(|r| r.contains(&unique)),
            "other agent must NOT see this confidential memory: {:?}", found
        );
    }

    /// Shared memory (empty owner_agent_id) is visible to everyone including anonymous.
    #[test]
    fn shared_memory_visible_to_all() {
        let unique = format!("shared_test_visible_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
        memory_store(&unique, "test", "", "shared")
            .expect("store should succeed");

        let found_anon = memory_search(&unique, 5, "")
            .expect("search should succeed");
        assert!(
            found_anon.iter().any(|r| r.contains(&unique)),
            "anonymous must see shared memory: {:?}", found_anon
        );
        let found_agent = memory_search(&unique, 5, "agent_isolation_e")
            .expect("search should succeed");
        assert!(
            found_agent.iter().any(|r| r.contains(&unique)),
            "agent must also see shared memory: {:?}", found_agent
        );
    }
}
