//! Periodic JPEG screenshot pump for the Live Monitor.
//!
//! Spawned as a tokio task by `monitor::spawn_screenshot_pump()`.
//! Only captures when `MonitorState::view_active()` is true and
//! `MonitorState::paused_stream()` is false — no CPU cost when nobody watches.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::monitor::state::MonitorState;

/// Default capture interval.
pub const DEFAULT_INTERVAL_MS: u64 = 500;

/// JPEG quality used for captured frames (1–100).
pub const JPEG_QUALITY: u8 = 80;

/// Main pump loop — runs forever until the task is dropped.
///
/// Call via `monitor::spawn_screenshot_pump()` rather than directly.
pub async fn run(state: Arc<MonitorState>, interval_ms: u64) {
    let interval = Duration::from_millis(interval_ms);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        // Skip if no one is watching
        if !state.view_active() {
            continue;
        }
        if state.paused_stream() {
            continue;
        }

        // screenshot_jpeg is synchronous — run in blocking thread
        let state_clone = Arc::clone(&state);
        let result = tokio::task::spawn_blocking(move || {
            crate::browser::screenshot_jpeg(JPEG_QUALITY)
        })
        .await;

        match result {
            Ok(Ok(jpeg_bytes)) => {
                state_clone.push_screenshot(Utc::now(), jpeg_bytes);
            }
            Ok(Err(e)) => {
                // Browser not open or other transient error — log quietly, don't crash
                tracing::debug!("[screenshot_pump] capture failed: {e}");
            }
            Err(join_err) => {
                tracing::warn!("[screenshot_pump] spawn_blocking panic: {join_err}");
            }
        }
    }
}
