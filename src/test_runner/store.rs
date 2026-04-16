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
            CREATE INDEX IF NOT EXISTS idx_tk_test ON test_knowledge(test_id); \
            CREATE TABLE IF NOT EXISTS auto_fix_history ( \
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                test_id TEXT NOT NULL, \
                run_id TEXT, \
                category TEXT NOT NULL, \
                triggered_at TEXT NOT NULL, \
                completed_at TEXT, \
                outcome TEXT NOT NULL, \
                claude_exit_code INTEGER, \
                claude_output TEXT, \
                bug_prompt TEXT, \
                verification_run_id TEXT, \
                verified_at TEXT \
            ); \
            CREATE INDEX IF NOT EXISTS idx_afh_test ON auto_fix_history(test_id, triggered_at); \
            CREATE INDEX IF NOT EXISTS idx_afh_outcome ON auto_fix_history(outcome);",
        )
        .expect("Failed to initialise test_memory schema");

        // Migration: pre-verification DBs are missing the new columns.
        // Ignore errors since they will fail "duplicate column" on fresh DBs.
        let _ = conn.execute("ALTER TABLE auto_fix_history ADD COLUMN verification_run_id TEXT", []);
        let _ = conn.execute("ALTER TABLE auto_fix_history ADD COLUMN verified_at TEXT", []);

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

/// Return the most recent test runs across ALL tests.  Used by the
/// `list_recent_runs` MCP endpoint when no test_id is specified.
pub fn recent_runs_all(limit: usize) -> Vec<RunRecord> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = match conn.prepare(
        "SELECT id, test_id, started_at, duration_ms, status, failure_category, ai_analysis, screenshot_path \
         FROM test_runs ORDER BY started_at DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(
        rusqlite::params![limit as i64],
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

// ── Auto-fix history ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FixRecord {
    pub id: i64,
    pub test_id: String,
    pub run_id: Option<String>,
    pub category: String,
    pub triggered_at: String,
    pub completed_at: Option<String>,
    /// pending → fix_attempted (claude returned, awaiting verification)
    /// → verified (re-ran test, passed) OR regressed (re-ran, still fails)
    /// failed: claude_session itself failed (non-zero exit)
    /// skipped_dedupe: not spawned because another fix is in flight
    pub outcome: String,
    pub claude_exit_code: Option<i64>,
    pub claude_output: Option<String>,
    pub verification_run_id: Option<String>,
    pub verified_at: Option<String>,
}

/// Record a new auto-fix attempt as "pending".  Returns the fix_id so the
/// caller can later call `complete_fix(fix_id, ...)` when the spawned Claude
/// session finishes.
pub fn record_pending_fix(
    test_id: &str,
    run_id: Option<&str>,
    category: &str,
    bug_prompt: &str,
) -> Result<i64, String> {
    let now = chrono::Local::now().to_rfc3339();
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.execute(
        "INSERT INTO auto_fix_history(test_id, run_id, category, triggered_at, outcome, bug_prompt) \
         VALUES (?1,?2,?3,?4,'pending',?5)",
        rusqlite::params![test_id, run_id, category, now, bug_prompt],
    ).map_err(|e| format!("record_pending_fix: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Mark a previously-pending fix as having received a claude_session response.
/// Outcomes:
/// - exit=0 → `fix_attempted` (awaiting verification re-run)
/// - exit!=0 → `failed` (claude_session itself errored)
pub fn complete_fix(
    fix_id: i64,
    exit_code: i64,
    output: &str,
) -> Result<(), String> {
    let now = chrono::Local::now().to_rfc3339();
    let outcome = if exit_code == 0 { "fix_attempted" } else { "failed" };
    let trimmed: String = output.chars().take(2000).collect();
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.execute(
        "UPDATE auto_fix_history SET completed_at=?1, outcome=?2, claude_exit_code=?3, claude_output=?4 \
         WHERE id=?5",
        rusqlite::params![now, outcome, exit_code, trimmed, fix_id],
    ).map_err(|e| format!("complete_fix: {e}"))?;
    Ok(())
}

/// Record the verification run that confirmed (or refuted) the fix.
/// `passed` true → outcome becomes `verified`, false → `regressed`.
pub fn record_verification(
    fix_id: i64,
    verification_run_id: &str,
    passed: bool,
) -> Result<(), String> {
    let now = chrono::Local::now().to_rfc3339();
    let outcome = if passed { "verified" } else { "regressed" };
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.execute(
        "UPDATE auto_fix_history SET verification_run_id=?1, verified_at=?2, outcome=?3 \
         WHERE id=?4",
        rusqlite::params![verification_run_id, now, outcome, fix_id],
    ).map_err(|e| format!("record_verification: {e}"))?;
    Ok(())
}

/// Returns true if there is a `pending` auto-fix for this test started within
/// the last `minutes` — used to deduplicate spawns.
pub fn has_pending_fix(test_id: &str, minutes: i64) -> bool {
    let cutoff = (chrono::Local::now() - chrono::Duration::minutes(minutes)).to_rfc3339();
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.query_row(
        "SELECT COUNT(*) FROM auto_fix_history \
         WHERE test_id=?1 AND outcome='pending' AND triggered_at > ?2",
        rusqlite::params![test_id, cutoff],
        |row| row.get::<_, i64>(0),
    ).map(|n| n > 0).unwrap_or(false)
}

const FIX_COLS: &str = "id, test_id, run_id, category, triggered_at, \
    completed_at, outcome, claude_exit_code, claude_output, \
    verification_run_id, verified_at";

fn map_fix_row(row: &rusqlite::Row) -> rusqlite::Result<FixRecord> {
    Ok(FixRecord {
        id: row.get(0)?,
        test_id: row.get(1)?,
        run_id: row.get(2)?,
        category: row.get(3)?,
        triggered_at: row.get(4)?,
        completed_at: row.get(5)?,
        outcome: row.get(6)?,
        claude_exit_code: row.get(7)?,
        claude_output: row.get(8)?,
        verification_run_id: row.get(9)?,
        verified_at: row.get(10)?,
    })
}

/// Return most recent fix attempts across ALL tests.
pub fn recent_fixes_all(limit: usize) -> Vec<FixRecord> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let sql = format!(
        "SELECT {FIX_COLS} FROM auto_fix_history ORDER BY triggered_at DESC LIMIT ?1"
    );
    let mut stmt = match conn.prepare(&sql) { Ok(s) => s, Err(_) => return Vec::new() };
    let rows = stmt.query_map(rusqlite::params![limit as i64], map_fix_row);
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// Return recent fix attempts for a test.
pub fn recent_fixes(test_id: &str, limit: usize) -> Vec<FixRecord> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let sql = format!(
        "SELECT {FIX_COLS} FROM auto_fix_history WHERE test_id=?1 ORDER BY triggered_at DESC LIMIT ?2"
    );
    let mut stmt = match conn.prepare(&sql) { Ok(s) => s, Err(_) => return Vec::new() };
    let rows = stmt.query_map(rusqlite::params![test_id, limit as i64], map_fix_row);
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// Mark a fix attempt as skipped due to an existing pending fix.
/// Still recorded so we can tell "we saw this bug and dedupe'd".
pub fn record_skipped_fix(
    test_id: &str,
    run_id: Option<&str>,
    category: &str,
    reason: &str,
) -> Result<(), String> {
    let now = chrono::Local::now().to_rfc3339();
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.execute(
        "INSERT INTO auto_fix_history(test_id, run_id, category, triggered_at, outcome, claude_output) \
         VALUES (?1,?2,?3,?4,'skipped_dedupe',?5)",
        rusqlite::params![test_id, run_id, category, now, reason],
    ).map_err(|e| format!("record_skipped_fix: {e}"))?;
    Ok(())
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
    fn fix_history_lifecycle() {
        let tid = format!("test_fix_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        // Initially no pending fix
        assert!(!has_pending_fix(&tid, 60));

        // Record pending
        let fix_id = record_pending_fix(&tid, Some("run_abc"), "ui_bug", "test bug prompt").unwrap();
        assert!(has_pending_fix(&tid, 60), "should detect pending fix");

        // Claude returned successfully
        complete_fix(fix_id, 0, "all good").unwrap();
        assert!(!has_pending_fix(&tid, 60), "fix_attempted shouldn't count as pending");

        // After complete_fix, outcome is 'fix_attempted' (awaiting verification)
        let fixes = recent_fixes(&tid, 10);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].outcome, "fix_attempted");
        assert_eq!(fixes[0].claude_exit_code, Some(0));
        assert!(fixes[0].verification_run_id.is_none());

        // Record verification — passed
        record_verification(fix_id, "ver_run_xyz", true).unwrap();
        let fixes = recent_fixes(&tid, 10);
        assert_eq!(fixes[0].outcome, "verified");
        assert_eq!(fixes[0].verification_run_id.as_deref(), Some("ver_run_xyz"));
        assert!(fixes[0].verified_at.is_some());
    }

    #[test]
    fn fix_history_regression_outcome() {
        let tid = format!("test_regr_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        let fix_id = record_pending_fix(&tid, None, "ui_bug", "bug").unwrap();
        complete_fix(fix_id, 0, "claude said done").unwrap();
        record_verification(fix_id, "ver_run_z", false).unwrap();
        let fixes = recent_fixes(&tid, 10);
        assert_eq!(fixes[0].outcome, "regressed");
    }

    /// Demonstrates the full auto-fix verification state machine without
    /// invoking real claude_session or browser.  Documents the expected
    /// transitions for future maintainers.
    ///
    /// State graph:
    ///   pending → fix_attempted → verified | regressed
    ///   pending → failed (claude_session non-zero exit)
    ///   pending → skipped_dedupe (recorded by record_skipped_fix)
    #[test]
    fn fix_state_machine_full_demo() {
        let base = chrono::Local::now().timestamp_nanos_opt().unwrap_or(0);

        // ── Path A: happy path — verified ─────────────────────────────────
        let tid_a = format!("demo_verified_{}", base);
        let fix_a = record_pending_fix(&tid_a, Some("run_xyz"), "ui_bug",
            "Test failed: button missing").unwrap();
        let r = recent_fixes(&tid_a, 1);
        assert_eq!(r[0].outcome, "pending", "spawn started");

        // claude_session returned exit=0 (claimed the fix is done)
        complete_fix(fix_a, 0, "I added the missing button").unwrap();
        let r = recent_fixes(&tid_a, 1);
        assert_eq!(r[0].outcome, "fix_attempted", "claude returned successfully");
        assert!(r[0].verification_run_id.is_none(), "no verification yet");

        // Sirin spawned a verification run, it passed
        record_verification(fix_a, "ver_run_aaa", true).unwrap();
        let r = recent_fixes(&tid_a, 1);
        assert_eq!(r[0].outcome, "verified",
            "test passed after fix → verified");
        assert_eq!(r[0].verification_run_id.as_deref(), Some("ver_run_aaa"));

        // ── Path B: regression — claude "fixed" but test still fails ─────
        let tid_b = format!("demo_regressed_{}", base + 1);
        let fix_b = record_pending_fix(&tid_b, None, "api_bug", "500 on POST").unwrap();
        complete_fix(fix_b, 0, "Updated handler").unwrap();
        record_verification(fix_b, "ver_run_bbb", false).unwrap();
        let r = recent_fixes(&tid_b, 1);
        assert_eq!(r[0].outcome, "regressed",
            "verified=false → regressed (escalate to human)");

        // ── Path C: claude_session itself failed ─────────────────────────
        let tid_c = format!("demo_failed_{}", base + 2);
        let fix_c = record_pending_fix(&tid_c, None, "ui_bug", "x").unwrap();
        complete_fix(fix_c, 1, "compile error in claude's patch").unwrap();
        let r = recent_fixes(&tid_c, 1);
        assert_eq!(r[0].outcome, "failed",
            "exit!=0 → failed (no verification attempted)");
        assert!(r[0].verification_run_id.is_none());

        // ── Path D: dedupe — second spawn while first is in flight ───────
        let tid_d = format!("demo_dedupe_{}", base + 3);
        let _fix_d1 = record_pending_fix(&tid_d, None, "ui_bug", "first").unwrap();
        assert!(has_pending_fix(&tid_d, 30));
        record_skipped_fix(&tid_d, None, "ui_bug",
            "another fix pending in last 30min").unwrap();
        let fixes = recent_fixes(&tid_d, 10);
        assert_eq!(fixes.len(), 2, "both pending and skipped recorded");
        assert!(fixes.iter().any(|f| f.outcome == "skipped_dedupe"));
        assert!(fixes.iter().any(|f| f.outcome == "pending"));
    }

    #[test]
    fn fix_history_failed_outcome() {
        let tid = format!("test_fix_fail_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        let fix_id = record_pending_fix(&tid, None, "api_bug", "bug").unwrap();
        complete_fix(fix_id, 1, "claude failed").unwrap();
        let fixes = recent_fixes(&tid, 10);
        assert_eq!(fixes[0].outcome, "failed");
        assert_eq!(fixes[0].claude_exit_code, Some(1));
    }

    #[test]
    fn fix_history_skipped_dedupe() {
        let tid = format!("test_skip_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        record_pending_fix(&tid, None, "ui_bug", "first").unwrap();
        record_skipped_fix(&tid, None, "ui_bug", "already pending").unwrap();
        let fixes = recent_fixes(&tid, 10);
        assert_eq!(fixes.len(), 2);
        assert!(fixes.iter().any(|f| f.outcome == "skipped_dedupe"));
    }

    #[test]
    fn has_pending_fix_respects_time_window() {
        let tid = format!("test_window_{}", chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
        record_pending_fix(&tid, None, "ui_bug", "bug").unwrap();
        // Very short window means nothing counts
        // Use 0 minutes (cutoff = now → anything triggered before now is too old)
        std::thread::sleep(std::time::Duration::from_millis(5));
        // 60 min window: should find it
        assert!(has_pending_fix(&tid, 60));
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
