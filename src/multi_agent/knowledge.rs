//! Squad Knowledge Base — persist PM lessons across restarts.
//!
//! PM includes `[📝 學到: <one-line lesson>]` in its reviews (see roles.rs).
//! After each successful task the worker parses these lines and writes them
//! to SQLite so they survive process restarts.
//!
//! Before each new task the relevant top-N lessons are injected into the
//! PM's first message so it doesn't re-learn the same things every session.
//!
//! ## Storage
//!
//! File: `<app_data_dir>/memory/squad_knowledge.db`
//! Table: `squad_knowledge (key, value, learned_at, source_task)`
//! Dedup: `key` is the first 80 chars of the lesson text — same lesson text
//! always maps to the same key, so repeated lessons overwrite rather than
//! accumulate.
//!
//! ## Relevance scoring
//!
//! Simple keyword overlap: split the task description into words ≥4 chars,
//! count how many appear in each lesson's key+value.  The top-N scorers are
//! returned.  If no words match, the most recent N lessons are returned as a
//! generic fallback.

use std::sync::{Mutex, OnceLock};
use crate::platform::app_data_dir;

// ── DB singleton ──────────────────────────────────────────────────────────────

fn db() -> &'static Mutex<rusqlite::Connection> {
    static DB: OnceLock<Mutex<rusqlite::Connection>> = OnceLock::new();
    DB.get_or_init(|| {
        let path = app_data_dir().join("memory").join("squad_knowledge.db");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = rusqlite::Connection::open(&path)
            .expect("open squad_knowledge.db");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS squad_knowledge ( \
                id          INTEGER PRIMARY KEY AUTOINCREMENT, \
                key         TEXT    NOT NULL UNIQUE, \
                value       TEXT    NOT NULL, \
                learned_at  TEXT    NOT NULL, \
                source_task TEXT \
            ); \
            CREATE INDEX IF NOT EXISTS idx_sk_learned \
                ON squad_knowledge(learned_at DESC);",
        ).expect("create squad_knowledge schema");
        Mutex::new(conn)
    })
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Extract `[📝 學到: <text>]` lines from a PM review reply.
///
/// Returns a `Vec` of plain lesson strings (no brackets, trimmed).
pub fn parse_lessons(review: &str) -> Vec<String> {
    review
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            // Accept both full-width and ASCII brackets, and optional trailing ']'
            let stripped = line
                .strip_prefix("[📝 學到:")
                .or_else(|| line.strip_prefix("[📝 學到："));  // full-width colon variant
            stripped.map(|rest| {
                rest.trim_end_matches(']').trim().to_string()
            })
            .filter(|s| !s.is_empty())
        })
        .collect()
}

// ── Storage ───────────────────────────────────────────────────────────────────

/// Persist a batch of lessons (from one task's review).
///
/// Deduplication: `key` = first 80 bytes of the lesson text.
/// Existing entries with the same key are overwritten (new source_task wins).
pub fn store_lessons(source_task: &str, lessons: &[String]) {
    if lessons.is_empty() {
        return;
    }
    let db = db();
    let conn = db.lock().unwrap_or_else(|e| e.into_inner());
    let now = chrono::Local::now().to_rfc3339();

    let mut stored = 0usize;
    for lesson in lessons {
        let text = lesson.trim();
        if text.is_empty() {
            continue;
        }
        // Key = first 80 chars (char-boundary safe)
        let key = {
            let max = text.len().min(80);
            let b = (0..=max).rev()
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(0);
            &text[..b]
        };

        if conn
            .execute(
                "INSERT INTO squad_knowledge (key, value, learned_at, source_task) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(key) DO UPDATE SET \
                   value       = excluded.value, \
                   learned_at  = excluded.learned_at, \
                   source_task = excluded.source_task",
                rusqlite::params![key, text, now, source_task],
            )
            .is_ok()
        {
            stored += 1;
        }
    }

    if stored > 0 {
        tracing::info!(target: "sirin",
            "[knowledge] Stored {stored} lesson(s) from task {source_task}");
    }
}

// ── Retrieval ─────────────────────────────────────────────────────────────────

/// Return the top-N lessons most relevant to a task description.
///
/// Relevance = number of task keywords (≥4 chars) found in `key + value`.
/// Ties are broken by recency (ORDER BY learned_at DESC in the underlying query).
///
/// Falls back to the most recent N lessons when no keyword matches are found.
/// Returns `Vec::new()` only when the knowledge base is empty.
pub fn relevant_lessons(task_description: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }

    let db = db();
    let conn = db.lock().unwrap_or_else(|e| e.into_inner());

    // Fetch a pool of recent lessons to score in Rust (avoids dynamic SQL)
    let pool_size = (limit * 6).max(30);
    let mut stmt = match conn.prepare(
        "SELECT key, value FROM squad_knowledge ORDER BY learned_at DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let pool: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![pool_size as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();

    if pool.is_empty() {
        return Vec::new();
    }

    // Keywords from task description (≥4 chars, lowercased)
    let keywords: Vec<String> = task_description
        .split_whitespace()
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_lowercase())
        .collect();

    if keywords.is_empty() {
        // No useful keywords → return most recent N (already sorted by recency)
        return pool.into_iter().take(limit).map(|(_, v)| v).collect();
    }

    // Score each (key, value) pair
    let mut scored: Vec<(usize, String)> = pool
        .into_iter()
        .map(|(key, value)| {
            let combined = format!("{key} {value}").to_lowercase();
            let score = keywords
                .iter()
                .filter(|kw| combined.contains(kw.as_str()))
                .count();
            (score, value)
        })
        .collect();

    // Sort by score descending (stable sort preserves recency order within ties)
    scored.sort_by(|a, b| b.0.cmp(&a.0));

    // Prefer matched; if nothing matched fall back to most-recent (score==0 entries)
    let has_match = scored.first().map(|(s, _)| *s > 0).unwrap_or(false);
    if has_match {
        scored
            .into_iter()
            .filter(|(s, _)| *s > 0)
            .take(limit)
            .map(|(_, v)| v)
            .collect()
    } else {
        // All scores are 0 → return most recent (original order before sort)
        scored.into_iter().take(limit).map(|(_, v)| v).collect()
    }
}

// ── Formatting ────────────────────────────────────────────────────────────────

/// Format lessons as a context block for the PM's task-planning message.
///
/// Returns an empty string (not a formatted block) when `lessons` is empty,
/// so callers can unconditionally prepend without producing stray blank lines.
pub fn format_knowledge_prefix(lessons: &[String]) -> String {
    if lessons.is_empty() {
        return String::new();
    }
    let items: Vec<String> = lessons.iter().map(|l| format!("• {l}")).collect();
    format!(
        "📚 過去學到的相關知識（請參考，避免重蹈覆轍）：\n{}\n\n",
        items.join("\n")
    )
}

// ── Status / debug ────────────────────────────────────────────────────────────

/// Retrieve all stored lessons for display (newest first).
/// Returns `(key, value, learned_at)` tuples.
pub fn all_lessons(limit: usize) -> Vec<(String, String, String)> {
    let db = db();
    let conn = db.lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = match conn.prepare(
        "SELECT key, value, learned_at FROM squad_knowledge \
         ORDER BY learned_at DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(rusqlite::params![limit as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// Total number of lessons stored.
pub fn lesson_count() -> usize {
    let db = db();
    let conn = db.lock().unwrap_or_else(|e| e.into_inner());
    conn.query_row("SELECT COUNT(*) FROM squad_knowledge", [], |row| {
        row.get::<_, i64>(0)
    })
    .map(|n| n as usize)
    .unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let review = "Good work!\n[📝 學到: 修改 browser.rs 後必須跑 cargo check]\n[PM ✓ 完成]";
        let lessons = parse_lessons(review);
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0], "修改 browser.rs 後必須跑 cargo check");
    }

    #[test]
    fn parse_multiple() {
        let review = "[📝 學到: lesson one]\n[📝 學到: lesson two]\n<<<VERDICT: APPROVED>>>";
        let lessons = parse_lessons(review);
        assert_eq!(lessons.len(), 2);
        assert_eq!(lessons[0], "lesson one");
        assert_eq!(lessons[1], "lesson two");
    }

    #[test]
    fn parse_none() {
        let review = "核准 <<<VERDICT: APPROVED>>>";
        assert!(parse_lessons(review).is_empty());
    }

    #[test]
    fn parse_full_width_colon() {
        let review = "[📝 學到：full-width colon variant]";
        let lessons = parse_lessons(review);
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0], "full-width colon variant");
    }

    #[test]
    fn format_prefix_empty() {
        assert!(format_knowledge_prefix(&[]).is_empty());
    }

    #[test]
    fn format_prefix_nonempty() {
        let prefix = format_knowledge_prefix(&["lesson one".to_string()]);
        assert!(prefix.contains("lesson one"));
        assert!(prefix.contains("📚"));
        assert!(prefix.contains("•"));
    }
}
