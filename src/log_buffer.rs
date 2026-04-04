//! Global in-process log ring buffer.
//!
//! All modules write via `sirin_log!(...)` which echoes to stderr and
//! appends to a fixed-size ring buffer that the egui UI reads every frame.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

const MAX_LINES: usize = 300;

static BUF: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

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
    }
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

/// Return the last `n` log lines joined as plain text.
pub fn snapshot_text(n: usize) -> String {
    recent(n).join("\n")
}

/// Clear the in-memory ring buffer.
pub fn clear() {
    if let Ok(mut b) = buf().lock() {
        b.clear();
    }
}

/// Log a message to both stderr and the UI ring buffer.
#[macro_export]
macro_rules! sirin_log {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        eprintln!("{}", msg);
        $crate::log_buffer::push(msg);
    }};
}
