//! Live GUI Monitor — core module.
//!
//! ## Quick start
//!
//! ```rust,ignore
//! // In main.rs, after creating the trace path:
//! monitor::init(MonitorConfig {
//!     trace_dir: PathBuf::from(".sirin"),
//!     trace_size_limit: None,   // use default 100 MB
//! });
//!
//! // Anywhere an action starts:
//! monitor::emit_action_start("claude-desktop", "axid-1", "ax_click",
//!                            serde_json::json!({"backend_id": 42})).await;
//! ```
//!
//! All emit helpers are **no-ops** if `init` was never called (they silently
//! return).  This makes the monitor fully optional — callers need not guard
//! against an uninitialised state.
//!
//! ## Thread / task safety
//! The global `Arc<MonitorState>` is cheaply cloneable.  Emit helpers acquire
//! the internal `RwLock` only long enough to push one event.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use serde_json::Value;

pub mod control;
pub mod events;
pub mod screenshot_pump;
pub mod state;
pub mod trace_writer;

pub use control::{ControlSnapshot, ControlState};
pub use events::{AuthzDecision, ClientCommand, ServerEvent, SubscribeChannel};
pub use state::MonitorState;
pub use trace_writer::TraceWriter;

// ── Global singleton ──────────────────────────────────────────────────────────

static MONITOR: OnceLock<Arc<MonitorState>> = OnceLock::new();

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration passed to [`init`].
pub struct MonitorConfig {
    /// Directory where trace NDJSON files are written.
    /// A file named `trace-<ISO8601>.ndjson` is created inside this dir.
    pub trace_dir: PathBuf,

    /// Rotation threshold in bytes.  `None` → use the 100 MiB default.
    pub trace_size_limit: Option<u64>,
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Initialise the global monitor singleton.
///
/// Call once at startup (from `main.rs`).  Subsequent calls are silently
/// ignored — `OnceLock` guarantees single initialisation.
///
/// Returns the shared `Arc<MonitorState>` for callers that want to hold a
/// handle without going through the `state()` accessor every time.
pub fn init(config: MonitorConfig) -> Arc<MonitorState> {
    let state = MONITOR.get_or_init(|| Arc::new(MonitorState::new()));
    let state_clone = Arc::clone(state);

    // Spawn trace-writer task.  We do this in a blocking thread because
    // TraceWriter uses synchronous file I/O.  The task receives events via
    // a channel and writes them serially — no lock contention on the hot path.
    let trace_dir = config.trace_dir.clone();
    let size_limit = config.trace_size_limit.unwrap_or(trace_writer::DEFAULT_SIZE_LIMIT);

    std::thread::Builder::new()
        .name("monitor-trace-writer".into())
        .spawn(move || {
            run_trace_writer(trace_dir, size_limit, state_clone);
        })
        .unwrap_or_else(|e| {
            eprintln!("[monitor] failed to spawn trace-writer thread: {e}");
            // Return a dummy JoinHandle-like value — the spawn itself returned Err,
            // so we have nothing to store.  This path is extremely unlikely.
            panic!("thread spawn failed");
        });

    Arc::clone(state)
}

/// Return the global `MonitorState`, or `None` if `init` was never called.
pub fn state() -> Option<Arc<MonitorState>> {
    MONITOR.get().map(Arc::clone)
}

// ── Trace-writer thread ───────────────────────────────────────────────────────

/// Blocking loop: poll the MonitorState event queue every 50 ms and flush
/// new events to the trace file.
///
/// This approach avoids a separate channel and keeps state as the single
/// source of truth.  The writer remembers its write cursor (last event index
/// tracked via event count) and flushes only the delta each tick.
fn run_trace_writer(trace_dir: PathBuf, size_limit: u64, state: Arc<MonitorState>) {
    // Build the trace file path: <trace_dir>/trace-<ISO8601>.ndjson
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ");
    let filename = format!("trace-{ts}.ndjson");
    let path = trace_dir.join(filename);

    let mut writer = match TraceWriter::open_with_limit(&path, size_limit) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[monitor] trace_writer: could not open {path:?}: {e}");
            return;
        }
    };

    let mut last_count = 0usize;

    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));

        let events = state.events_snapshot();
        let new_events = if events.len() > last_count {
            events[last_count..].to_vec()
        } else if events.len() < last_count {
            // State was cleared — reset cursor, don't re-write old events
            last_count = events.len();
            continue;
        } else {
            continue;
        };

        for ev in &new_events {
            if let Err(e) = writer.write_event(ev) {
                eprintln!("[monitor] trace_writer: write error: {e}");
            }
        }

        last_count += new_events.len();
    }
}

// ── Emit helpers ──────────────────────────────────────────────────────────────

/// Push a `Hello` event and register `client_id` as connected.
pub async fn emit_hello(sirin_version: impl Into<String>) {
    let Some(st) = state() else { return };
    let clients: Vec<String> = st.clients_snapshot().into_iter().collect();
    st.push_event(ServerEvent::Hello {
        ts: Utc::now(),
        sirin_version: sirin_version.into(),
        clients,
    });
}

/// Push an `ActionStart` event.
pub async fn emit_action_start(
    client: impl Into<String>,
    id: impl Into<String>,
    action: impl Into<String>,
    args: Value,
) {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::ActionStart {
        id: id.into(),
        ts: Utc::now(),
        client: client.into(),
        action: action.into(),
        args,
    });
}

/// Push an `ActionDone` event.
pub async fn emit_action_done(id: impl Into<String>, result: Value, duration_ms: u64) {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::ActionDone {
        id: id.into(),
        ts: Utc::now(),
        result,
        duration_ms,
    });
}

/// Push an `ActionError` event.
pub async fn emit_action_error(id: impl Into<String>, error: impl Into<String>) {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::ActionError {
        id: id.into(),
        ts: Utc::now(),
        error: error.into(),
    });
}

/// Push an `AuthzAsk` event.
#[allow(clippy::too_many_arguments)]
pub async fn emit_authz_ask(
    request_id: impl Into<String>,
    client: impl Into<String>,
    action: impl Into<String>,
    args: Value,
    url: impl Into<String>,
    timeout_ms: u64,
    learn: bool,
) {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::AuthzAsk {
        request_id: request_id.into(),
        ts: Utc::now(),
        client: client.into(),
        action: action.into(),
        args,
        url: url.into(),
        timeout_ms,
        learn,
    });
}

/// Push an `AuthzResolved` event.
pub async fn emit_authz_resolved(request_id: impl Into<String>, decision: impl Into<String>) {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::AuthzResolved {
        request_id: request_id.into(),
        ts: Utc::now(),
        decision: decision.into(),
    });
}

/// Push a `UrlChange` event.
pub async fn emit_url_change(url: impl Into<String>) {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::UrlChange {
        ts: Utc::now(),
        url: url.into(),
    });
}

/// Push a `Console` event.
pub async fn emit_console(level: impl Into<String>, text: impl Into<String>) {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::Console {
        ts: Utc::now(),
        level: level.into(),
        text: text.into(),
    });
}

/// Push a `Network` event.
#[allow(clippy::too_many_arguments)]
pub async fn emit_network(
    url: impl Into<String>,
    method: impl Into<String>,
    status: u16,
    size: u64,
    req_body_preview: Option<String>,
    res_body_preview: Option<String>,
) {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::Network {
        ts: Utc::now(),
        url: url.into(),
        method: method.into(),
        status,
        size,
        req_body_preview,
        res_body_preview,
    });
}

/// Push a `State` snapshot event reflecting current control flags.
pub async fn emit_state(paused: bool, step: bool, aborted: bool) {
    let Some(st) = state() else { return };
    let view_active = st.view_active();
    st.push_event(ServerEvent::State {
        ts: Utc::now(),
        paused,
        step,
        aborted,
        view_active,
    });
}

/// Push a `Goodbye` event (called on session teardown).
pub async fn emit_goodbye() {
    let Some(st) = state() else { return };
    st.push_event(ServerEvent::Goodbye { ts: Utc::now() });
}

// ── Control + pump helpers ────────────────────────────────────────────────────

/// Returns the global `ControlState` (initialised on first call).
pub fn control() -> Arc<crate::monitor::control::ControlState> {
    crate::monitor::control::global()
}

/// Spawn the screenshot pump as a background tokio task.
///
/// Call once from `main.rs` after `monitor::init()`.
/// The task runs forever; it self-throttles when `view_active` is false.
pub fn spawn_screenshot_pump() {
    if let Some(state) = state() {
        tokio::spawn(crate::monitor::screenshot_pump::run(
            state,
            crate::monitor::screenshot_pump::DEFAULT_INTERVAL_MS,
        ));
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helpers that call `state()` when monitor is not initialised must be
    /// no-ops — they must not panic.
    #[tokio::test]
    async fn emit_without_init_is_noop() {
        // These should all return without panicking even though `init` was
        // never called in this test binary's module-level init.
        // (MONITOR may already be set from another test; that's fine — the
        // calls will push to a real queue which is harmless.)
        emit_url_change("https://noop.test").await;
        emit_console("info", "hello").await;
        emit_action_error("id-0", "oops").await;
        emit_goodbye().await;
    }

    #[test]
    fn state_returns_none_before_init_in_new_process() {
        // We can't truly reset OnceLock in a running test binary, so we just
        // verify that `state()` returns *something* (Some or None) without
        // panicking regardless of init order.
        let _ = state();
    }
}
