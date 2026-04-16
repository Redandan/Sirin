//! SQLite storage for test runs + learned knowledge.
//!
//! Schema:
//! - `test_runs` — every execution (passed/failed/timeout/error) with analysis
//! - `test_knowledge` — flaky patterns, selector mappings, learned hints
//!
//! ## Concurrency
//! OnceLock-wrapped Mutex<Connection> — serialises writes, safe to share.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// ── DB init ──────────────────────────────────────────────────────────────────

fn db_path() -> PathBuf {
    crate::platform::app_data_dir().join("memory").join("test_memory.db")
}

fn db() -> &'static Mutex<rusqlite::Connection> {
    static DB: OnceLock<Mutex<rusqlite::Connection>> = OnceLock::new();
    DB.get_or_init(|| {
        let path = db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = rusqlite::Connection::open(&path)
            .expect("Failed to open test_memory.db");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS test_runs ( \
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                test_id TEXT NOT NULL, \
                started_at TEXT NOT NULL, \
                duration_ms INTEGER, \
                status TEXT NOT NULL, \
                failure_category TEXT, \
                ai_analysis TEXT, \
                screenshot_path TEXT, \
                history_json TEXT \
            ); \
            CREATE INDEX IF NOT EXISTS idx_tr_test ON test_runs(test_id, started_at); \
            CREATE TABLE IF NOT EXISTS test_knowledge ( \
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                test_id TEXT NOT NULL, \
                key TEXT NOT NULL, \
                value TEXT NOT NULL, \
                updated_at TEXT NOT NULL, \
                UNIQUE(test_id, key) \
            ); \
            CREATE INDEX IF NOT EXISTS idx_tk_test ON test_knowledge(test_id);",
        )
        .expect("Failed to initialise test_memory schema");
        Mutex::new(conn)
    })
}

// ── Record types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RunRecord {
    pub id: i64,
    pub test_id: String,
    pub started_at: String,
    pub duration_ms: Option<i64>,
    pub status: String,
    pub failure_category: Option<String>,
    pub ai_analysis: Option<String>,
    pub screenshot_path: Option<String>,
}

// ── API ──────────────────────────────────────────────────────────────────────

pub struct NewRun<'a> {
    pub test_id: &'a str,
    pub started_at: &'a str,
    pub duration_ms: Option<i64>,
    pub status: &'a str,
    pub failure_category: Option<&'a str>,
    pub ai_analysis: Option<&'a str>,
    pub screenshot_path: Option<&'a str>,
    pub history_json: Option<&'a str>,
}

pub fn record_run(r: NewRun<'_>) -> Result<i64, String> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.execute(
        "INSERT INTO test_runs(test_id, started_at, duration_ms, status, failure_category, ai_analysis, screenshot_path, history_json) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![
            r.test_id, r.started_at, r.duration_ms, r.status,
            r.failure_category, r.ai_analysis, r.screenshot_path, r.history_json
        ],
    ).map_err(|e| format!("insert run: {e}"))?;
    Ok(conn.last_insert_rowid())
}

pub fn recent_runs(test_id: &str, limit: usize) -> Vec<RunRecord> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = match conn.prepare(
        "SELECT id, test_id, started_at, duration_ms, status, failure_category, ai_analysis, screenshot_path \
         FROM test_runs WHERE test_id = ?1 ORDER BY started_at DESC LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(
        rusqlite::params![test_id, limit as i64],
        |row| Ok(RunRecord {
            id: row.get(0)?,
            test_id: row.get(1)?,
            started_at: row.get(2)?,
            duration_ms: row.get(3)?,
            status: row.get(4)?,
            failure_category: row.get(5)?,
            ai_analysis: row.get(6)?,
            screenshot_path: row.get(7)?,
        }),
    );
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// Ratio of passed runs over the last `limit` runs (0.0–1.0).
pub fn success_rate(test_id: &str, limit: usize) -> f64 {
    let runs = recent_runs(test_id, limit);
    if runs.is_empty() { return 1.0; }
    let passed = runs.iter().filter(|r| r.status == "passed").count();
    passed as f64 / runs.len() as f64
}

/// A test is considered flaky if it fails sometimes but not always over recent runs.
pub fn is_flaky(test_id: &str) -> bool {
    let runs = recent_runs(test_id, 10);
    if runs.len() < 3 { return false; }
    let passed = runs.iter().filter(|r| r.status == "passed").count();
    let failed = runs.len() - passed;
    // Flaky = both passes and fails exist within last 10 runs, with <70% pass rate
    passed > 0 && failed > 0 && (passed as f64 / runs.len() as f64) < 0.70
}

pub fn store_knowledge(test_id: &str, key: &str, value: &str) -> Result<(), String> {
    let now = chrono::Local::now().to_rfc3339();
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.execute(
        "INSERT INTO test_knowledge(test_id, key, value, updated_at) \
         VALUES (?1,?2,?3,?4) \
         ON CONFLICT(test_id,key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        rusqlite::params![test_id, key, value, now],
    ).map_err(|e| format!("store_knowledge: {e}"))?;
    Ok(())
}

pub fn get_knowledge(test_id: &str, key: &str) -> Option<String> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.query_row(
        "SELECT value FROM test_knowledge WHERE test_id=?1 AND key=?2",
        rusqlite::params![test_id, key],
        |row| row.get(0),
    ).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_many(test_id: &str, statuses: &[&str]) {
        for s in statuses {
            let now = chrono::Local::now().to_rfc3339();
            record_run(NewRun {
                test_id,
                started_at: &now,
                duration_ms: Some(100),
                status: s,
                failure_category: None,
                ai_analysis: None,
                screenshot_path: None,
                history_json: None,
            }).unwrap();
            // Small delay so started_at ordering is deterministic
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn record_and_retrieve_runs() {
        let tid = format!("test_record_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        insert_many(&tid, &["passed", "failed", "passed"]);
        let runs = recent_runs(&tid, 10);
        assert_eq!(runs.len(), 3);
        // Most recent first
        assert_eq!(runs[0].status, "passed");
    }

    #[test]
    fn success_rate_calculation() {
        let tid = format!("test_rate_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        insert_many(&tid, &["passed", "passed", "failed", "passed"]);
        let rate = success_rate(&tid, 10);
        assert!((rate - 0.75).abs() < 0.01, "expected 0.75, got {rate}");
    }

    #[test]
    fn flaky_detection() {
        let tid_flaky = format!("test_flaky_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        insert_many(&tid_flaky, &["passed", "failed", "passed", "failed", "failed"]);
        assert!(is_flaky(&tid_flaky), "should be flaky with mixed results");

        let tid_stable = format!("test_stable_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0) + 1);
        insert_many(&tid_stable, &["passed", "passed", "passed", "passed"]);
        assert!(!is_flaky(&tid_stable), "all-pass should not be flaky");
    }

    #[test]
    fn knowledge_upsert() {
        let tid = format!("test_kn_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        store_knowledge(&tid, "selector_login", "#login-btn").unwrap();
        assert_eq!(get_knowledge(&tid, "selector_login"), Some("#login-btn".into()));
        // Upsert replaces
        store_knowledge(&tid, "selector_login", "button[aria-label=登入]").unwrap();
        assert_eq!(get_knowledge(&tid, "selector_login"), Some("button[aria-label=登入]".into()));
        assert_eq!(get_knowledge(&tid, "nonexistent"), None);
    }
}
