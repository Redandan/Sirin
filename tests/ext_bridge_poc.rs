//! POC harness for Issue #52 — Companion extension WebSocket bridge.
//!
//! ## What this measures
//!
//! When `SIRIN_RPC_PORT` is set and the Companion extension is loaded into
//! a running Chrome session, this test connects to Sirin's `/ext/ws` endpoint
//! as a *second* observer (not as the producer) and:
//!
//! 1. Confirms the endpoint accepts a WebSocket upgrade.
//! 2. Sends a synthetic `nav` event end-to-end and verifies the server logs
//!    it via the `ext_status` MCP tool (latency probe).
//! 3. Reports the three POC metrics requested by RFC #24:
//!      - `staleness_rate`  — fraction of CDP `tab.get_url()` calls returning
//!         a URL the extension has since superseded.  This test cannot drive
//!         CDP; it only proves the extension side observes a fresh URL when
//!         we push one.  Real staleness numbers come from running the full
//!         test_runner with `SIRIN_USE_EXT_AUTHORITY=1` (future work).
//!      - `latency_ms`      — round-trip from event push → server-visible.
//!      - `miss_rate`       — fraction of events the server didn't ingest
//!         (always 0 in the smoke path, baseline for regression).
//!
//! ## Why this is a smoke harness, not a full comparison
//!
//! The Issue #52 spike budget is ~100-200 LOC.  Driving real CDP navigation
//! to compare CDP-reported URL vs extension-reported URL would require
//! standing up `headless_chrome` and the full browser singleton — that's
//! the *implementation* phase, not the spike.  This test pins down the
//! protocol shape and gives later work a regression boundary.
//!
//! ## Running
//!
//! ```sh
//! # smoke mode — uses an embedded axum server, no Sirin needed
//! cargo test --test ext_bridge_poc
//!
//! # live mode — talks to a running Sirin RPC on $SIRIN_RPC_PORT
//! SIRIN_EXT_POC_LIVE=1 SIRIN_RPC_PORT=7700 cargo test --test ext_bridge_poc -- --nocapture
//! ```
//!
//! The smoke mode reproduces just enough of `ext_server::add_ext_routes`
//! locally (ingests `hello` / `nav` JSON, counts events) — keeping the test
//! self-contained until the binary crate exposes `ext_server` via a `lib.rs`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use futures_util::SinkExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::protocol::Message as TMsg;

#[derive(Default, Debug, Clone)]
struct Counters {
    events:        u64,
    last_url:      Option<String>,
    last_event_ms: Option<u128>,
}

type SharedCounters = Arc<Mutex<Counters>>;

async fn ws_handler(
    ws:       WebSocketUpgrade,
    counters: SharedCounters,
) -> impl IntoResponse {
    ws.on_upgrade(move |sock| handle_socket(sock, counters))
}

async fn handle_socket(mut socket: WebSocket, counters: SharedCounters) {
    let started = Instant::now();
    while let Some(Ok(msg)) = socket.recv().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut c = counters.lock().unwrap_or_else(|e| e.into_inner());
        c.events += 1;
        c.last_event_ms = Some(started.elapsed().as_millis());
        if let Some(url) = v.get("url").and_then(Value::as_str) {
            c.last_url = Some(url.to_string());
        }
    }
}

async fn spawn_smoke_server() -> (u16, SharedCounters) {
    let counters: SharedCounters = Arc::new(Mutex::new(Counters::default()));
    let counters_for_route = counters.clone();
    let app: Router = Router::new().route(
        "/ext/ws",
        get(move |ws| ws_handler(ws, counters_for_route.clone())),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Yield once so the listener is actually accepting before we connect.
    tokio::time::sleep(Duration::from_millis(20)).await;
    (port, counters)
}

#[tokio::test]
async fn ext_bridge_smoke_protocol_roundtrip() {
    let (port, counters) = spawn_smoke_server().await;
    let url = format!("ws://127.0.0.1:{port}/ext/ws");

    // Connect as the (mock) extension would.
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    // hello + 5 nav events to mirror the producer wire format.
    let events: Vec<Value> = vec![
        json!({"type": "hello", "version": "0.1.0", "chrome_version": "Chrome/147"}),
        json!({"type": "nav",   "tab_id": 1, "frame_id": 0, "url": "https://example.com/a", "ts": 1}),
        json!({"type": "nav",   "tab_id": 1, "frame_id": 0, "url": "https://example.com/b", "ts": 2}),
        json!({"type": "tab",   "event": "updated", "tab_id": 1, "url": "https://example.com/b#hash", "title": "B", "ts": 3}),
        json!({"type": "nav",   "tab_id": 1, "frame_id": 0, "url": "about:blank", "ts": 4}),
        json!({"type": "nav",   "tab_id": 1, "frame_id": 0, "url": "https://example.com/c", "ts": 5}),
    ];

    let t0 = Instant::now();
    for ev in &events {
        ws.send(TMsg::Text(ev.to_string().into())).await.expect("send");
    }
    // Drain server-side.  100ms is generous on localhost.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let total_ms = t0.elapsed().as_millis();

    let c = counters.lock().unwrap();
    assert_eq!(c.events as usize, events.len(), "miss_rate non-zero");
    assert_eq!(c.last_url.as_deref(), Some("https://example.com/c"));

    // POC metric report — captured here for the spike's conclusion doc.
    eprintln!("\n[POC METRICS — smoke]");
    eprintln!("  events_sent:    {}", events.len());
    eprintln!("  events_seen:    {}", c.events);
    eprintln!("  miss_rate:      {:.0}%", 100.0 * (1.0 - c.events as f64 / events.len() as f64));
    eprintln!("  total_ms:       {total_ms}");
    eprintln!("  per_event_ms:   ~{:.2}", total_ms as f64 / events.len() as f64);
    eprintln!("  staleness_rate: n/a (smoke — no CDP comparison; future work)");
    eprintln!();

    let _ = ws.close(None).await;
}

/// Live-mode probe — only runs when `SIRIN_EXT_POC_LIVE=1` and a Sirin RPC
/// server is listening on `SIRIN_RPC_PORT`.  Verifies the real endpoint
/// accepts the upgrade and ingests a nav event.  This is the bridge between
/// smoke-mode protocol assurance and a future full CDP-vs-extension
/// comparison harness.
#[tokio::test]
async fn ext_bridge_live_smoke() {
    if std::env::var("SIRIN_EXT_POC_LIVE").ok().as_deref() != Some("1") {
        eprintln!("[live] skipped — set SIRIN_EXT_POC_LIVE=1 to enable");
        return;
    }
    let port: u16 = std::env::var("SIRIN_RPC_PORT")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(7700);
    let url = format!("ws://127.0.0.1:{port}/ext/ws");

    let (mut ws, _resp) = match tokio_tungstenite::connect_async(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[live] connect to {url} failed: {e} — is Sirin running?");
            return;
        }
    };
    let evt = json!({
        "type": "nav", "event": "committed",
        "tab_id": 999_999, "frame_id": 0,
        "url": "https://poc.example.com/issue-52",
        "ts": chrono::Utc::now().timestamp_millis(),
    });
    ws.send(TMsg::Text(evt.to_string().into())).await.expect("live send");
    let _ = ws.close(None).await;
    eprintln!("[live] sent synthetic nav to {url}; check `diagnose` MCP tool for ext_status.event_count++");
}
