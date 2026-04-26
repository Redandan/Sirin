//! Telegram Bot API notification hook for test failures.
//!
//! Uses the stateless Telegram Bot HTTP API, **not** MTProto — no session
//! required.  Requires two env vars to be active:
//!
//! | Env var                  | Value                                    |
//! |--------------------------|------------------------------------------|
//! | `SIRIN_NOTIFY_BOT_TOKEN` | Telegram bot token from \@BotFather      |
//! | `SIRIN_NOTIFY_CHAT_ID`   | Target chat/user ID (integer as string)  |
//!
//! If either var is unset or empty, notifications are silently skipped —
//! callers never see an error.  Send failures are logged as `warn` only.
//!
//! ## Dedup + batch digest (Issue #38)
//!
//! Concurrent regression runs used to spawn N independent threads, each
//! firing one TG message — the chat got flooded.  Two protections:
//!
//! 1. **Per-test dedup window** (`NOTIFY_DEDUP_WINDOW_SECS`, default 300s).
//!    A second failure of the same `test_id` within the window is silently
//!    dropped.
//! 2. **Batch digest** — after `spawn_batch_run` finishes, the caller
//!    invokes [`notify_batch_complete`] which sends a single summary line
//!    instead of K individual failure pings.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Per-test "last notified at" cache.  Keyed by `test_id`.
/// `std::sync::Mutex<HashMap>` keeps Cargo.toml dep-free; the lock is held
/// for microseconds so contention is irrelevant.
fn last_notified() -> &'static Mutex<HashMap<String, Instant>> {
    static CELL: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn dedup_window() -> Duration {
    let secs = std::env::var("NOTIFY_DEDUP_WINDOW_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    Duration::from_secs(secs)
}

/// Returns `true` if a notification for `test_id` should be sent now,
/// `false` if the last send was within the dedup window.
fn check_and_mark(test_id: &str) -> bool {
    let now = Instant::now();
    let window = dedup_window();
    let mut map = last_notified().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prev) = map.get(test_id) {
        if now.duration_since(*prev) < window {
            return false;
        }
    }
    map.insert(test_id.to_string(), now);
    true
}

fn telegram_creds() -> Option<(String, String)> {
    let token = std::env::var("SIRIN_NOTIFY_BOT_TOKEN").ok().filter(|t| !t.is_empty())?;
    let chat_id = std::env::var("SIRIN_NOTIFY_CHAT_ID").ok().filter(|c| !c.is_empty())?;
    Some((token, chat_id))
}

fn spawn_send(token: String, chat_id: String, msg: String) {
    std::thread::spawn(move || {
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        if let Err(e) = reqwest::blocking::Client::new()
            .post(&url)
            .json(&serde_json::json!({ "chat_id": chat_id, "text": msg }))
            .send()
        {
            tracing::warn!(target: "sirin", "[notify] Telegram send failed: {e}");
        }
    });
}

/// Fire-and-forget: notify a Telegram chat when a test fails.
///
/// Spawns a background thread so `record_run` (the caller) is not blocked.
/// Returns immediately.  Never panics.  Safe to call from sync context.
///
/// Skips silently if the same `test_name` was notified less than the dedup
/// window ago (default 5 min, override via `NOTIFY_DEDUP_WINDOW_SECS`).
pub fn notify_failure(test_name: &str, reason: &str, duration_ms: u64) {
    if !check_and_mark(test_name) {
        tracing::debug!(target: "sirin",
            "[notify] dedup: skipping repeat failure for '{test_name}'");
        return;
    }
    let Some((token, chat_id)) = telegram_creds() else { return };
    let msg = format!(
        "Test FAILED: {test_name} | {reason} | Duration: {duration_ms}ms"
    );
    spawn_send(token, chat_id, msg);
}

/// One result line in a batch summary.
pub struct BatchResultLine<'a> {
    pub test_id: &'a str,
    pub passed: bool,
    pub reason: Option<&'a str>,
}

/// Send a single TG message summarising a batch of test runs.
///
/// Designed for `spawn_batch_run` callers: `results` should contain one
/// entry per test in the batch (regardless of pass/fail).  When all tests
/// passed the digest is still sent — the user sees one green line instead
/// of silence.  Does not consult the dedup map (the digest itself is
/// strictly one-per-batch by construction).
pub fn notify_batch_complete(results: &[BatchResultLine<'_>]) {
    if results.is_empty() { return; }
    let Some((token, chat_id)) = telegram_creds() else { return };

    let total = results.len();
    let failed: Vec<&BatchResultLine<'_>> = results.iter().filter(|r| !r.passed).collect();
    let passed = total - failed.len();

    let mut msg = format!(
        "Batch done: {total} tests | {passed} passed | {fail} failed",
        fail = failed.len()
    );
    if !failed.is_empty() {
        let names: Vec<String> = failed.iter().take(10).map(|r| {
            match r.reason {
                Some(why) => format!("{} ({})", r.test_id, why),
                None      => r.test_id.to_string(),
            }
        }).collect();
        msg.push_str("\nFailed: ");
        msg.push_str(&names.join(", "));
        if failed.len() > 10 {
            msg.push_str(&format!(" … (+{} more)", failed.len() - 10));
        }
    }

    spawn_send(token, chat_id, msg);
}

/// Format a weekly test-health digest from current `store::all_test_stats()`.
///
/// Pure formatter — no I/O.  Returned `None` when there is no run history
/// at all, so callers can skip empty TG sends.
pub fn format_weekly_digest() -> Option<String> {
    let stats = crate::test_runner::store::all_test_stats();
    if stats.is_empty() { return None; }
    let total = stats.len();
    let flaky_count = stats.iter().filter(|s| s.is_flaky).count();
    let avg_pass: f64 = stats.iter().map(|s| s.pass_rate_7d).sum::<f64>() / total as f64;
    let worst: Vec<&crate::test_runner::store::TestStats> = stats.iter()
        .filter(|s| s.pass_rate_7d < 0.7)
        .take(3)
        .collect();

    let mut msg = format!(
        "Weekly Test Health | {total} tests | avg pass {:.0}% | {flaky_count} flaky",
        avg_pass * 100.0,
    );
    if !worst.is_empty() {
        msg.push_str("\nWorst:");
        for s in &worst {
            msg.push_str(&format!(
                "\n  {} {:.0}% ({} runs)",
                s.test_id,
                s.pass_rate_7d * 100.0,
                s.total_runs,
            ));
        }
    }
    Some(msg)
}

/// Fire-and-forget weekly digest send.  Same env-var contract as
/// `notify_failure` — silently skipped when SIRIN_NOTIFY_BOT_TOKEN /
/// SIRIN_NOTIFY_CHAT_ID are unset.  Caller decides scheduling (no cron
/// framework wired here yet — see Issue #35 follow-up).
pub fn weekly_test_digest() {
    let Some(msg) = format_weekly_digest() else { return; };
    let token = match std::env::var("SIRIN_NOTIFY_BOT_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return,
    };
    let chat_id = match std::env::var("SIRIN_NOTIFY_CHAT_ID") {
        Ok(c) if !c.is_empty() => c,
        _ => return,
    };
    std::thread::spawn(move || {
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        if let Err(e) = reqwest::blocking::Client::new()
            .post(&url)
            .json(&serde_json::json!({ "chat_id": chat_id, "text": msg }))
            .send()
        {
            tracing::warn!(target: "sirin", "[notify] weekly digest send failed: {e}");
        }
    });
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// When neither env var is set, notify_failure must return silently —
    /// no panic, no blocking, no network call.
    #[test]
    fn test_no_chat_no_panic() {
        notify_failure("dummy_test", "assertion failed", 42);
    }

    /// Only token set, no chat_id → returns silently without network call.
    #[test]
    fn test_token_without_chat_id_no_panic() {
        notify_failure("test_b", "timeout after 120s", 120_000);
    }

    /// First call returns true (send), second within window returns false.
    #[test]
    fn test_dedup_blocks_repeat() {
        // Use a unique key so parallel test runs don't collide.
        let id = "dedup_unit_test_unique_xyz_42";
        assert!(check_and_mark(id), "first call should send");
        assert!(!check_and_mark(id), "second call within window should skip");
    }

    /// Empty batch → no-op (no panic, no message).
    #[test]
    fn test_batch_empty_noop() {
        notify_batch_complete(&[]);
    }

    /// Batch digest formats correctly when env vars are absent (no send).
    #[test]
    fn test_batch_digest_no_creds_no_panic() {
        let lines = vec![
            BatchResultLine { test_id: "a", passed: true,  reason: None },
            BatchResultLine { test_id: "b", passed: false, reason: Some("UiBug") },
        ];
        notify_batch_complete(&lines);
    }
}
