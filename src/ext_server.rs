//! Sirin Companion extension WebSocket endpoint (POC).
//!
//! ## Why this exists
//!
//! CDP-attach (the way `headless_chrome` talks to Chrome) is *cooperative*,
//! not authoritative — Chrome may change tab state without firing the events
//! we subscribe to.  Issues #18 / #20 / #21 / #23 are all symptoms of this:
//! `tab.get_url()` returns a stale CDP cache long after the page navigated
//! to `about:blank`, and our agents trust it.
//!
//! The Companion extension lives *inside* Chrome and uses the
//! `chrome.tabs.*` / `chrome.webNavigation.*` APIs that are owned by the
//! browser process — so they're ground truth by construction.  It pushes
//! every navigation / tab lifecycle event over a WebSocket to this module.
//!
//! ## Wire format
//!
//! See `ext/background.js` for the producer side.  The Rust side accepts
//! anything; we record only the fields we care about and ignore the rest.
//!
//! ## Usage
//!
//! - [`add_ext_routes`] — mount `/ext/ws` on the existing rpc Router.
//! - [`status`]            — for the `diagnose` MCP tool (connected? last event?).
//! - [`authoritative_url`] — what `current_url()` *should* return when the
//!   extension is alive (POC: caller decides; future work hooks this into
//!   `browser::current_url`).
//! - [`list_tabs`]         — list every tab the browser knows about.
//!
//! ## POC scope
//!
//! This module **does not** modify any existing Sirin behaviour — it only
//! observes.  A follow-up commit will wire `browser::current_url` /
//! `page_title` to prefer the extension's truth when available, falling
//! back to CDP cache otherwise (the "barbell" architecture from the RFC).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use serde::Serialize;
use serde_json::Value;

// ── State ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize)]
pub struct TabInfo {
    pub tab_id:    i64,
    pub url:       Option<String>,
    pub title:     Option<String>,
    pub status:    Option<String>,
    pub active:    bool,
    pub window_id: Option<i64>,
    /// Monotonic timestamp of the last event that touched this tab (ms since
    /// process start). Used to detect "is this tab info recent?"
    pub last_seen_ms: u64,
}

#[derive(Default)]
struct ExtState {
    connected:      bool,
    connected_at:   Option<Instant>,
    last_event_at:  Option<Instant>,
    event_count:    u64,
    chrome_version: Option<String>,
    ext_version:    Option<String>,
    tabs:           HashMap<i64, TabInfo>,
    /// Last activated tab_id (extension pushes `tab.event=activated`).
    active_tab:     Option<i64>,
}

fn state() -> &'static Mutex<ExtState> {
    static S: OnceLock<Mutex<ExtState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(ExtState::default()))
}

fn epoch_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as u64
}

// ── Public read API (consumed by diagnose / browser fallback / MCP) ──────────

/// Lightweight status snapshot for the `diagnose` MCP tool.
#[derive(Debug, Clone, Serialize)]
pub struct ExtStatus {
    pub connected:           bool,
    pub uptime_secs:         Option<u64>,
    pub last_event_age_secs: Option<u64>,
    pub event_count:         u64,
    pub tab_count:           usize,
    pub active_tab_id:       Option<i64>,
    pub chrome_version:      Option<String>,
    pub ext_version:         Option<String>,
}

pub fn status() -> ExtStatus {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    ExtStatus {
        connected:           s.connected,
        uptime_secs:         s.connected_at.map(|t| t.elapsed().as_secs()),
        last_event_age_secs: s.last_event_at.map(|t| t.elapsed().as_secs()),
        event_count:         s.event_count,
        tab_count:           s.tabs.len(),
        active_tab_id:       s.active_tab,
        chrome_version:      s.chrome_version.clone(),
        ext_version:         s.ext_version.clone(),
    }
}

/// Authoritative URL for the given tab id, or for the active tab when
/// `tab_id` is `None`.  Returns `None` when the extension hasn't reported
/// this tab yet (caller should fall back to CDP cache).
#[allow(dead_code)]
pub fn authoritative_url(tab_id: Option<i64>) -> Option<String> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let id = tab_id.or(s.active_tab)?;
    s.tabs.get(&id).and_then(|t| t.url.clone())
}

/// Authoritative title for the given tab id, or for the active tab when
/// `tab_id` is `None`.
#[allow(dead_code)]
pub fn authoritative_title(tab_id: Option<i64>) -> Option<String> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let id = tab_id.or(s.active_tab)?;
    s.tabs.get(&id).and_then(|t| t.title.clone())
}

/// Snapshot every tab the extension currently knows about.
#[allow(dead_code)]
pub fn list_tabs() -> Vec<TabInfo> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<_> = s.tabs.values().cloned().collect();
    v.sort_by_key(|t| t.tab_id);
    v
}

// ── Routing ──────────────────────────────────────────────────────────────────

/// Add `/ext/ws` route to the rpc Router.  Call from `start_rpc_server`.
pub fn add_ext_routes(router: Router) -> Router {
    router.route("/ext/ws", get(ws_upgrade_handler))
}

async fn ws_upgrade_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    tracing::info!("[ext] extension connecting");
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    {
        let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
        s.connected = true;
        s.connected_at = Some(Instant::now());
    }
    tracing::info!("[ext] extension connected");

    while let Some(Ok(msg)) = socket.recv().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        if let Err(e) = handle_message(text.as_str()) {
            tracing::warn!("[ext] message handler error: {e}");
        }
    }

    {
        let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
        s.connected = false;
    }
    tracing::info!("[ext] extension disconnected");
}

// ── Message dispatch ─────────────────────────────────────────────────────────

fn handle_message(raw: &str) -> Result<(), String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("json: {e}"))?;
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");

    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.event_count = s.event_count.saturating_add(1);
    s.last_event_at = Some(Instant::now());

    match ty {
        "hello" => {
            s.ext_version    = v.get("version").and_then(Value::as_str).map(String::from);
            s.chrome_version = v.get("chrome_version").and_then(Value::as_str).map(String::from);
            tracing::info!(
                "[ext] hello — ext v{} chrome={}",
                s.ext_version.as_deref().unwrap_or("?"),
                s.chrome_version.as_deref().unwrap_or("?"),
            );
        }
        "pong" => { /* keep-alive — already updated last_event_at above */ }
        "tab" => {
            let event  = v.get("event").and_then(Value::as_str).unwrap_or("");
            let tab_id = v.get("tab_id").and_then(Value::as_i64);
            let Some(id) = tab_id else { return Ok(()); };
            match event {
                "removed" => {
                    s.tabs.remove(&id);
                    if s.active_tab == Some(id) { s.active_tab = None; }
                }
                "activated" => {
                    s.active_tab = Some(id);
                }
                _ => {
                    let entry = s.tabs.entry(id).or_insert_with(|| TabInfo { tab_id: id, ..Default::default() });
                    if let Some(u) = v.get("url").and_then(Value::as_str)    { entry.url    = Some(u.into()); }
                    if let Some(t) = v.get("title").and_then(Value::as_str)  { entry.title  = Some(t.into()); }
                    if let Some(st) = v.get("status").and_then(Value::as_str){ entry.status = Some(st.into()); }
                    if let Some(a) = v.get("active").and_then(Value::as_bool){ entry.active = a; }
                    if let Some(w) = v.get("window_id").and_then(Value::as_i64){ entry.window_id = Some(w); }
                    entry.last_seen_ms = epoch_ms();
                }
            }
        }
        "nav" => {
            // Navigation events update URL aggressively — this is the
            // payoff for the whole extension (catches about:blank reset,
            // hash changes, history pushState that CDP misses).
            let tab_id = v.get("tab_id").and_then(Value::as_i64);
            let url    = v.get("url").and_then(Value::as_str);
            let frame  = v.get("frame_id").and_then(Value::as_i64).unwrap_or(0);
            if let (Some(id), Some(u)) = (tab_id, url) {
                if frame == 0 {  // main frame only
                    let entry = s.tabs.entry(id).or_insert_with(|| TabInfo { tab_id: id, ..Default::default() });
                    entry.url = Some(u.into());
                    entry.last_seen_ms = epoch_ms();
                    tracing::debug!("[ext] nav tab={id} url={u}");
                }
            }
        }
        _ => { /* unknown — log and ignore */ }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fresh_state() {
        let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
        *s = ExtState::default();
    }

    #[test]
    fn handles_hello_records_versions() {
        fresh_state();
        let _ = handle_message(&json!({
            "type": "hello", "version": "0.1.0", "chrome_version": "Chrome/147"
        }).to_string());
        let st = status();
        assert_eq!(st.ext_version.as_deref(), Some("0.1.0"));
        assert_eq!(st.chrome_version.as_deref(), Some("Chrome/147"));
        assert_eq!(st.event_count, 1);
    }

    #[test]
    fn handles_tab_updated_then_removed() {
        fresh_state();
        let _ = handle_message(&json!({
            "type": "tab", "event": "updated", "tab_id": 42,
            "url": "https://example.com/foo", "title": "Foo", "active": true
        }).to_string());
        assert_eq!(authoritative_url(Some(42)).as_deref(), Some("https://example.com/foo"));
        assert_eq!(authoritative_title(Some(42)).as_deref(), Some("Foo"));
        assert_eq!(list_tabs().len(), 1);

        let _ = handle_message(&json!({
            "type": "tab", "event": "removed", "tab_id": 42
        }).to_string());
        assert_eq!(list_tabs().len(), 0);
        assert!(authoritative_url(Some(42)).is_none());
    }

    #[test]
    fn nav_event_updates_url_immediately() {
        fresh_state();
        // Initial state from tab.updated
        let _ = handle_message(&json!({
            "type": "tab", "event": "updated", "tab_id": 7,
            "url": "https://app.example.com/#/dashboard"
        }).to_string());
        // Page resets to about:blank (the #23 scenario)
        let _ = handle_message(&json!({
            "type": "nav", "event": "committed", "tab_id": 7,
            "frame_id": 0, "url": "about:blank"
        }).to_string());
        // Authoritative URL must reflect the reset, not the cached SPA URL
        assert_eq!(authoritative_url(Some(7)).as_deref(), Some("about:blank"));
    }

    #[test]
    fn activated_event_sets_active_tab() {
        fresh_state();
        let _ = handle_message(&json!({
            "type": "tab", "event": "updated", "tab_id": 1, "url": "https://a"
        }).to_string());
        let _ = handle_message(&json!({
            "type": "tab", "event": "updated", "tab_id": 2, "url": "https://b"
        }).to_string());
        let _ = handle_message(&json!({
            "type": "tab", "event": "activated", "tab_id": 2
        }).to_string());
        // No tab_id → use active
        assert_eq!(authoritative_url(None).as_deref(), Some("https://b"));
    }

    #[test]
    fn nav_in_subframe_does_not_change_top_url() {
        fresh_state();
        let _ = handle_message(&json!({
            "type": "tab", "event": "updated", "tab_id": 9, "url": "https://main"
        }).to_string());
        // Subframe navigation should NOT clobber the top-level URL
        let _ = handle_message(&json!({
            "type": "nav", "event": "committed", "tab_id": 9, "frame_id": 12,
            "url": "https://ad-subframe.example.com"
        }).to_string());
        assert_eq!(authoritative_url(Some(9)).as_deref(), Some("https://main"));
    }
}
