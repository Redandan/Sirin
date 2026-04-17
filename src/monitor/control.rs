//! Pause / Step / Abort control state for the Live Monitor.
//!
//! `ControlState` is a lock-free triplet of atomics that gate action execution.
//! Wire it into `call_browser_exec` in `mcp_server.rs` (after authz, before exec):
//!
//! ```rust,ignore
//! monitor::control().gate().await?;
//! ```

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

// ── Singleton ─────────────────────────────────────────────────────────────────

static CONTROL: OnceLock<Arc<ControlState>> = OnceLock::new();

/// Returns the global `ControlState`, initialising it on first call.
pub fn global() -> Arc<ControlState> {
    Arc::clone(CONTROL.get_or_init(|| Arc::new(ControlState::new())))
}

// ── ControlState ──────────────────────────────────────────────────────────────

/// Three-flag atomic gate used to pause, step-through, or abort ongoing
/// browser actions driven by an external MCP client.
pub struct ControlState {
    /// When `true`, every incoming action blocks until `paused` becomes `false`
    /// (or `aborted` becomes `true`).
    pub paused: AtomicBool,

    /// When `true`, the *next* action is allowed through, then `paused` is set
    /// back to `true` automatically (single-step mode).
    pub step: AtomicBool,

    /// When `true`, all subsequent `gate()` calls return `Err` immediately.
    /// Once set, cannot be unset — caller must reset the whole session.
    pub aborted: AtomicBool,
}

impl ControlState {
    pub fn new() -> Self {
        Self {
            paused:  AtomicBool::new(false),
            step:    AtomicBool::new(false),
            aborted: AtomicBool::new(false),
        }
    }

    /// Reset all flags to their default (running) state.
    /// Call at the start of a new MCP session.
    pub fn reset(&self) {
        self.aborted.store(false, Relaxed);
        self.paused.store(false, Relaxed);
        self.step.store(false, Relaxed);
    }

    /// Gate an incoming action.
    ///
    /// - If `aborted` → returns `Err("session aborted")` immediately.
    /// - If `step` → clears `step`, allows this action through, then sets `paused=true`.
    /// - If `paused` (and not step) → polls every 50 ms until unpaused or aborted.
    /// - Otherwise → returns `Ok(())` immediately.
    pub async fn gate(&self) -> Result<(), String> {
        if self.aborted.load(Relaxed) {
            return Err("session aborted by operator".into());
        }
        // Step mode: allow this one action through, then re-pause for the next
        if self.step.swap(false, Relaxed) {
            self.paused.store(true, Relaxed);
            return Ok(());
        }
        while self.paused.load(Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if self.aborted.load(Relaxed) {
                return Err("session aborted by operator".into());
            }
        }
        Ok(())
    }

    /// Current state snapshot for UI / WS emission.
    pub fn snapshot(&self) -> ControlSnapshot {
        ControlSnapshot {
            paused:  self.paused.load(Relaxed),
            step:    self.step.load(Relaxed),
            aborted: self.aborted.load(Relaxed),
        }
    }
}

impl Default for ControlState {
    fn default() -> Self { Self::new() }
}

/// Immutable snapshot of `ControlState` suitable for JSON serialisation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ControlSnapshot {
    pub paused:  bool,
    pub step:    bool,
    pub aborted: bool,
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gate_passes_when_idle() {
        let c = ControlState::new();
        assert!(c.gate().await.is_ok());
    }

    #[tokio::test]
    async fn gate_fails_immediately_when_aborted() {
        let c = ControlState::new();
        c.aborted.store(true, Relaxed);
        assert!(c.gate().await.is_err());
    }

    #[tokio::test]
    async fn step_mode_auto_pauses_after_one_action() {
        let c = ControlState::new();
        c.paused.store(true, Relaxed);
        c.step.store(true, Relaxed);
        // gate() should pass (step clears pause for this action)
        assert!(c.gate().await.is_ok());
        // Now paused again
        assert!(c.paused.load(Relaxed));
        assert!(!c.step.load(Relaxed));
    }

    #[test]
    fn reset_clears_all_flags() {
        let c = ControlState::new();
        c.aborted.store(true, Relaxed);
        c.paused.store(true, Relaxed);
        c.step.store(true, Relaxed);
        c.reset();
        assert!(!c.aborted.load(Relaxed));
        assert!(!c.paused.load(Relaxed));
        assert!(!c.step.load(Relaxed));
    }
}
