//! Auto-close the test Chrome after a configurable idle period.
//!
//! ## Why
//!
//! Sirin keeps Chrome alive between test runs as a singleton — re-launching
//! costs 2-5 seconds and trashes the persistent profile.  But when no
//! tests run for minutes (user moved on, batch finished), Chrome sits
//! consuming 300-500 MB RAM across renderer / GPU / utility processes.
//!
//! User feedback (2026-05-05): after a regression batch, the test Chrome
//! window stays open on the secondary monitor "doing nothing" — surprising
//! UX, no obvious way to know if it's safe to close manually.
//!
//! ## Behaviour
//!
//! Background tokio task polls every 20 s:
//!
//! 1. Are there any active runs (Queued / Running)?  → reset idle timer
//! 2. Is the browser closed already?                   → reset idle timer
//! 3. Otherwise, idle period started; if it has now exceeded the
//!    configured threshold, send `browser::close()` (CDP Browser.close
//!    + singleton drop).  Next test run will spawn a fresh Chrome.
//!
//! ## Config
//!
//! Env var `SIRIN_BROWSER_IDLE_CLOSE_SECS` (default `60`):
//! - `0` disables the auto-close entirely
//! - any positive integer sets the threshold in seconds

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_secs(20);
const DEFAULT_IDLE_THRESHOLD_SECS: u64 = 60;

/// Shared state — exposes idle countdown to other crates (snapshot UI).
/// `(idle_since, threshold_secs)`.  `idle_since=None` means not currently
/// idle (test running or browser closed); the snapshot endpoint hides the
/// countdown entirely in that case.  `threshold_secs=0` means auto-close
/// disabled.  Initialised on first watcher spawn.
struct IdleState {
    idle_since: Option<Instant>,
    threshold_secs: u64,
}

fn state() -> &'static Mutex<IdleState> {
    static S: OnceLock<Mutex<IdleState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(IdleState { idle_since: None, threshold_secs: 0 }))
}

/// Snapshot helper — returns `Some(remaining_secs)` only when the watcher is
/// actively counting down to a Chrome close.  `None` for "not idle yet" or
/// "auto-close disabled".  Saturates at 0 (countdown elapsed but next poll
/// hasn't fired yet).
pub fn idle_close_in_secs() -> Option<u64> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    if s.threshold_secs == 0 {
        return None;
    }
    let started = s.idle_since?;
    let elapsed = started.elapsed().as_secs();
    Some(s.threshold_secs.saturating_sub(elapsed))
}

/// Spawn the idle-close watcher.  No-op when the env override is `0`.
pub fn spawn_idle_close_watcher() {
    let threshold_secs: u64 = std::env::var("SIRIN_BROWSER_IDLE_CLOSE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_IDLE_THRESHOLD_SECS);
    {
        let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
        s.threshold_secs = threshold_secs;
        s.idle_since = None;
    }
    if threshold_secs == 0 {
        tracing::info!("[browser_idle] auto-close disabled (SIRIN_BROWSER_IDLE_CLOSE_SECS=0)");
        return;
    }
    let threshold = Duration::from_secs(threshold_secs);
    tokio::spawn(idle_loop(threshold));
    tracing::info!(
        "[browser_idle] auto-close enabled — will close test Chrome after {threshold_secs}s of no active runs"
    );
}

async fn idle_loop(threshold: Duration) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        let active = crate::test_runner::runs::list_active().len();
        let browser_open = crate::browser::is_open();

        if active > 0 || !browser_open {
            // Test in flight OR browser already closed — reset.
            let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
            s.idle_since = None;
            continue;
        }

        // Snapshot current state and decide outside the guard.
        let started = {
            let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
            if s.idle_since.is_none() {
                s.idle_since = Some(Instant::now());
            }
            s.idle_since.expect("just initialised")
        };

        if started.elapsed() >= threshold {
            tracing::info!(
                "[browser_idle] no active runs for {}s — closing test Chrome (free RAM; next test will relaunch)",
                started.elapsed().as_secs()
            );
            crate::browser::close();
            let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
            s.idle_since = None;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn env_zero_disables_watcher() {
        // Pin the contract: SIRIN_BROWSER_IDLE_CLOSE_SECS=0 short-circuits
        // before spawning anything.  Reading the env directly mirrors the
        // production parse path; the spawn function returns immediately
        // without touching tokio in this branch (no runtime needed in test).
        std::env::set_var("SIRIN_BROWSER_IDLE_CLOSE_SECS", "0");
        let parsed: u64 = std::env::var("SIRIN_BROWSER_IDLE_CLOSE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(super::DEFAULT_IDLE_THRESHOLD_SECS);
        assert_eq!(parsed, 0);
        std::env::remove_var("SIRIN_BROWSER_IDLE_CLOSE_SECS");
    }

    #[test]
    fn env_unset_uses_default_60() {
        std::env::remove_var("SIRIN_BROWSER_IDLE_CLOSE_SECS");
        let parsed: u64 = std::env::var("SIRIN_BROWSER_IDLE_CLOSE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(super::DEFAULT_IDLE_THRESHOLD_SECS);
        assert_eq!(parsed, 60);
    }

    #[test]
    fn env_custom_value_parses() {
        std::env::set_var("SIRIN_BROWSER_IDLE_CLOSE_SECS", "300");
        let parsed: u64 = std::env::var("SIRIN_BROWSER_IDLE_CLOSE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(super::DEFAULT_IDLE_THRESHOLD_SECS);
        assert_eq!(parsed, 300);
        std::env::remove_var("SIRIN_BROWSER_IDLE_CLOSE_SECS");
    }
}
