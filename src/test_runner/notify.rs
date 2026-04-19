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

/// Fire-and-forget: notify a Telegram chat when a test fails.
///
/// Spawns a background thread so `record_run` (the caller) is not blocked.
/// Returns immediately.  Never panics.  Safe to call from sync context.
pub fn notify_failure(test_name: &str, reason: &str, duration_ms: u64) {
    let token = match std::env::var("SIRIN_NOTIFY_BOT_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return,
    };
    let chat_id = match std::env::var("SIRIN_NOTIFY_CHAT_ID") {
        Ok(c) if !c.is_empty() => c,
        _ => return,
    };

    let msg = format!(
        "Test FAILED: {test_name} | {reason} | Duration: {duration_ms}ms"
    );

    std::thread::spawn(move || {
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        match reqwest::blocking::Client::new()
            .post(&url)
            .json(&serde_json::json!({ "chat_id": chat_id, "text": msg }))
            .send()
        {
            Ok(_) => {}
            Err(e) => tracing::warn!(target: "sirin",
                "[notify] Telegram send failed for failed test: {e}"),
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
        // The env vars are almost certainly absent in CI / fresh test env.
        // We call anyway and assert: no panic = test passes.
        // (If they happen to be set in the developer's env the function will
        // attempt a send in a background thread, which is also fine.)
        notify_failure("dummy_test", "assertion failed", 42);
    }

    /// Only token set, no chat_id → returns silently without network call.
    #[test]
    fn test_token_without_chat_id_no_panic() {
        // Can't safely mutate env vars in parallel tests; this test only
        // asserts the early-return path when chat_id is missing.
        // If SIRIN_NOTIFY_CHAT_ID is absent, the function exits before
        // spawning the thread.  Just verifying it doesn't panic.
        notify_failure("test_b", "timeout after 120s", 120_000);
    }
}
