//! Shared monitor state: event queue, screenshot buffer, client registry.
//!
//! `MonitorState` is an `Arc<RwLock<MonitorStateInner>>` newtype. All writer
//! methods live on `MonitorState` (take `&self`) and acquire the write-lock
//! internally, so callers never need to hold the lock directly.
//!
//! ## Eviction policy
//! - Events: retain the most recent 1 000.
//! - Screenshots: retain the most recent 50.
//!
//! This caps memory at roughly 5 MB per the DESIGN_MONITOR §8 budget.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};

use super::events::ServerEvent;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const MAX_EVENTS: usize = 1_000;
pub const MAX_SCREENSHOTS: usize = 50;

// ── Inner state ───────────────────────────────────────────────────────────────

/// The mutable interior of `MonitorState`.
pub struct MonitorStateInner {
    /// Circular event log — newest at the back.
    pub events: VecDeque<ServerEvent>,

    /// Recent screenshots `(captured_at, jpeg_bytes)` — newest at the back.
    pub screenshots: VecDeque<(DateTime<Utc>, Vec<u8>)>,

    /// True while the Monitor egui view is open and visible.
    pub view_active: bool,

    /// True when the screenshot pump should skip taking frames
    /// (user hit "Pause stream" in the screenshot pane).
    pub paused_stream: bool,

    /// Set of currently-connected client IDs.
    pub clients: HashSet<String>,
}

impl MonitorStateInner {
    fn new() -> Self {
        Self {
            events: VecDeque::with_capacity(MAX_EVENTS),
            screenshots: VecDeque::with_capacity(MAX_SCREENSHOTS),
            view_active: false,
            paused_stream: false,
            clients: HashSet::new(),
        }
    }
}

// ── Public newtype ────────────────────────────────────────────────────────────

/// Thread-safe, cheaply-cloneable monitor state.
#[derive(Clone)]
pub struct MonitorState(Arc<RwLock<MonitorStateInner>>);

impl MonitorState {
    /// Create a fresh, empty state.
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(MonitorStateInner::new())))
    }

    // ── Read access ──────────────────────────────────────────────────────────

    /// Snapshot all current events (cloned).
    pub fn events_snapshot(&self) -> Vec<ServerEvent> {
        let inner = self.0.read().unwrap_or_else(|e| e.into_inner());
        inner.events.iter().cloned().collect()
    }

    /// Latest screenshot, if any.
    pub fn latest_screenshot(&self) -> Option<(DateTime<Utc>, Vec<u8>)> {
        let inner = self.0.read().unwrap_or_else(|e| e.into_inner());
        inner.screenshots.back().cloned()
    }

    pub fn view_active(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).view_active
    }

    pub fn paused_stream(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).paused_stream
    }

    pub fn clients_snapshot(&self) -> HashSet<String> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clients.clone()
    }

    // ── Write access ─────────────────────────────────────────────────────────

    /// Append an event, evicting the oldest when the queue is full.
    pub fn push_event(&self, event: ServerEvent) {
        let mut inner = self.0.write().unwrap_or_else(|e| e.into_inner());
        if inner.events.len() >= MAX_EVENTS {
            inner.events.pop_front();
        }
        inner.events.push_back(event);
    }

    /// Append a screenshot frame, evicting the oldest when the buffer is full.
    pub fn push_screenshot(&self, ts: DateTime<Utc>, jpeg_bytes: Vec<u8>) {
        let mut inner = self.0.write().unwrap_or_else(|e| e.into_inner());
        if inner.screenshots.len() >= MAX_SCREENSHOTS {
            inner.screenshots.pop_front();
        }
        inner.screenshots.push_back((ts, jpeg_bytes));
    }

    /// Record whether the Monitor view is currently open.
    pub fn set_view_active(&self, active: bool) {
        self.0.write().unwrap_or_else(|e| e.into_inner()).view_active = active;
    }

    /// Pause or resume the screenshot stream (does not affect action gating).
    pub fn set_paused_stream(&self, paused: bool) {
        self.0.write().unwrap_or_else(|e| e.into_inner()).paused_stream = paused;
    }

    /// Register or unregister a client ID.
    pub fn mark_client(&self, client_id: &str, connected: bool) {
        let mut inner = self.0.write().unwrap_or_else(|e| e.into_inner());
        if connected {
            inner.clients.insert(client_id.to_owned());
        } else {
            inner.clients.remove(client_id);
        }
    }

    /// Clear the event queue and screenshot buffer (e.g. user pressed "Clear").
    pub fn clear(&self) {
        let mut inner = self.0.write().unwrap_or_else(|e| e.into_inner());
        inner.events.clear();
        inner.screenshots.clear();
    }

    /// Number of events currently held.
    pub fn event_count(&self) -> usize {
        self.0.read().unwrap_or_else(|e| e.into_inner()).events.len()
    }

    /// Number of screenshots currently held.
    pub fn screenshot_count(&self) -> usize {
        self.0.read().unwrap_or_else(|e| e.into_inner()).screenshots.len()
    }
}

impl Default for MonitorState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_event(n: u32) -> ServerEvent {
        ServerEvent::UrlChange {
            ts: Utc::now(),
            url: format!("https://example.com/{n}"),
        }
    }

    // ── Event queue eviction ─────────────────────────────────────────────────

    #[test]
    fn event_queue_evicts_oldest_when_full() {
        let state = MonitorState::new();

        // Push MAX_EVENTS events — URLs /0 … /999
        for i in 0..MAX_EVENTS as u32 {
            state.push_event(make_event(i));
        }
        assert_eq!(state.event_count(), MAX_EVENTS);

        // Push one more — should drop URL /0, keep /1 … /1000
        state.push_event(make_event(MAX_EVENTS as u32));
        assert_eq!(state.event_count(), MAX_EVENTS, "queue must stay at capacity");

        let events = state.events_snapshot();
        let first_url = match &events[0] {
            ServerEvent::UrlChange { url, .. } => url.clone(),
            _ => panic!("unexpected variant"),
        };
        assert_eq!(
            first_url, "https://example.com/1",
            "oldest event was not evicted"
        );

        let last_url = match events.last().unwrap() {
            ServerEvent::UrlChange { url, .. } => url.clone(),
            _ => panic!("unexpected variant"),
        };
        assert_eq!(
            last_url,
            format!("https://example.com/{}", MAX_EVENTS),
            "newest event is wrong"
        );
    }

    #[test]
    fn event_queue_below_capacity_no_eviction() {
        let state = MonitorState::new();
        for i in 0..10u32 {
            state.push_event(make_event(i));
        }
        assert_eq!(state.event_count(), 10);
    }

    // ── Screenshot queue eviction ────────────────────────────────────────────

    #[test]
    fn screenshot_queue_evicts_oldest_when_full() {
        let state = MonitorState::new();

        for i in 0..MAX_SCREENSHOTS {
            state.push_screenshot(Utc::now(), vec![i as u8]);
        }
        assert_eq!(state.screenshot_count(), MAX_SCREENSHOTS);

        // Push one more
        state.push_screenshot(Utc::now(), vec![0xFF]);
        assert_eq!(
            state.screenshot_count(),
            MAX_SCREENSHOTS,
            "screenshot queue must stay at capacity"
        );

        // The latest frame should be our 0xFF sentinel
        let (_, latest) = state.latest_screenshot().unwrap();
        assert_eq!(latest, vec![0xFF], "latest screenshot is wrong after eviction");
    }

    #[test]
    fn screenshot_queue_below_capacity_no_eviction() {
        let state = MonitorState::new();
        state.push_screenshot(Utc::now(), vec![1, 2, 3]);
        state.push_screenshot(Utc::now(), vec![4, 5, 6]);
        assert_eq!(state.screenshot_count(), 2);
    }

    // ── Flag mutations ───────────────────────────────────────────────────────

    #[test]
    fn view_active_toggle() {
        let state = MonitorState::new();
        assert!(!state.view_active());
        state.set_view_active(true);
        assert!(state.view_active());
        state.set_view_active(false);
        assert!(!state.view_active());
    }

    #[test]
    fn paused_stream_toggle() {
        let state = MonitorState::new();
        assert!(!state.paused_stream());
        state.set_paused_stream(true);
        assert!(state.paused_stream());
    }

    // ── Client registry ──────────────────────────────────────────────────────

    #[test]
    fn client_connect_disconnect() {
        let state = MonitorState::new();
        state.mark_client("claude-desktop", true);
        state.mark_client("claude-code", true);
        assert_eq!(state.clients_snapshot().len(), 2);

        state.mark_client("claude-desktop", false);
        assert_eq!(state.clients_snapshot().len(), 1);
        assert!(state.clients_snapshot().contains("claude-code"));
    }

    // ── Clear ────────────────────────────────────────────────────────────────

    #[test]
    fn clear_resets_queues() {
        let state = MonitorState::new();
        state.push_event(make_event(0));
        state.push_screenshot(Utc::now(), vec![1]);
        state.clear();
        assert_eq!(state.event_count(), 0);
        assert_eq!(state.screenshot_count(), 0);
    }
}
