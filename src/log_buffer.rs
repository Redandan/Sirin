//! Global in-process log ring buffer.
//!
//! ## Concurrency
//! Ring-buffer is a `Mutex<VecDeque<String>>`.  `append` / `recent` / `clear`
//! all take the lock briefly.  `version()` uses an atomic counter so the UI's
//! polling loop can cheaply detect "something new" without taking the mutex.
//!
//! All modules write via `sirin_log!(...)` which echoes to stderr and
//! appends to a fixed-size ring buffer that the UI reads on demand.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const MAX_LINES: usize = 300;

static BUF: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
/// Monotonically increasing counter bumped on every push/clear.
/// Used by the UI log cache to detect when refiltering is needed.
static VERSION: AtomicUsize = AtomicUsize::new(0);

fn buf() -> &'static Mutex<VecDeque<String>> {
    BUF.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)))
}

/// Append a log line. Called by the `sirin_log!` macro.
pub fn push(msg: String) {
    if let Ok(mut b) = buf().lock() {
        if b.len() >= MAX_LINES {
            b.pop_front();
        }
        b.push_back(msg);
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Current buffer version — bumped on every `push()` or `clear()`.
pub fn version() -> usize {
    VERSION.load(Ordering::Relaxed)
}

/// Number of lines currently in the buffer.
pub fn len() -> usize {
    buf().lock().map(|b| b.len()).unwrap_or(0)
}

/// Return the last `n` log lines, oldest first.
pub fn recent(n: usize) -> Vec<String> {
    buf()
        .lock()
        .map(|b| {
            let skip = b.len().saturating_sub(n);
            b.iter().skip(skip).cloned().collect()
        })
        .unwrap_or_default()
}


/// Clear the in-memory ring buffer.
pub fn clear() {
    if let Ok(mut b) = buf().lock() {
        b.clear();
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Log a message to both stderr and the UI ring buffer.
///
/// Implemented as a thin wrapper around `tracing::info!` so the tracing
/// subscriber installed in `main.rs` handles the stderr write and the
/// [`crate::log_subscriber::LogBufferLayer`] handles the ring-buffer push.
/// New code should prefer `tracing::info!` / `warn!` / `error!` directly to
/// get level selection, plus `info_span!` around async tasks for correlation.
#[macro_export]
macro_rules! sirin_log {
    ($($arg:tt)*) => {{
        ::tracing::info!(target: "sirin", $($arg)*);
    }};
}
