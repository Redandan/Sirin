//! Local WebSocket RPC server + MCP HTTP endpoint on `127.0.0.1:<port>`.
//!
//! Port is `7700` by default; override via `SIRIN_RPC_PORT` env var (useful
//! when the previous Sirin left a zombie socket or another process is
//! holding the port).
//!
//! Accepts JSON-RPC-style messages and dispatches to internal Sirin functions:
//! - `memory_search`    → [`crate::memory::memory_search`]
//! - `call_graph_query` → [`crate::code_graph::query_call_graph`]
//! - `trigger_research` → [`crate::events::publish`] ResearchRequested
//!
//! # Message format
//! ```json
//! { "method": "memory_search", "params": { "query": "...", "limit": 5 } }
//! ```
//! # Response format
//! ```json
//! { "result": { ... } }
//! { "error": "..." }
//! ```

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use serde_json::{json, Value};

/// Default port when `SIRIN_RPC_PORT` is unset.
pub const DEFAULT_RPC_PORT: u16 = 7700;

static RUNNING: AtomicBool = AtomicBool::new(false);
/// The port the server actually bound to (0 = not yet bound).
static ACTIVE_PORT: AtomicU16 = AtomicU16::new(0);

/// Resolve the configured port from `SIRIN_RPC_PORT` env var, falling back
/// to the default.  Returns `None` if the env var is present but unparseable.
pub fn configured_port() -> u16 {
    std::env::var("SIRIN_RPC_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_RPC_PORT)
}

/// Returns `true` once the server has bound and started accepting connections.
pub fn is_running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// Returns the port the server is currently listening on, or `None` if not yet bound.
#[allow(dead_code)]
pub fn active_port() -> Option<u16> {
    let p = ACTIVE_PORT.load(Ordering::Relaxed);
    if p == 0 { None } else { Some(p) }
}

/// Bind the WebSocket + MCP server and serve forever. Spawn this as a Tokio task.
///
/// ## Port selection
/// Tries `SIRIN_RPC_PORT` (default 7700) first.  On bind failure, walks up
/// to [`MAX_PORT_FALLBACK`] sequential ports (e.g. 7700 → 7701 → 7702 → 7703)
/// before giving up.  Each individual port is retried twice with a 1-second
/// backoff to handle TCP TIME_WAIT churn.
///
/// Why fallback instead of just retrying: when 7700 is held by a stuck
/// previous Sirin instance the user can't kill, retrying the same port for
/// 6 seconds then dying is unhelpful — fallback ports let us self-heal.
/// The chosen port is recorded in [`active_port`] so external clients can
/// discover it via `diagnose.identity.rpc_port`.
pub async fn start_rpc_server() {
    let app = crate::ext_server::add_ext_routes(
        Router::new()
            .route("/", get(ws_upgrade_handler))
            .merge(crate::mcp_server::mcp_router()),
    );

    let primary = configured_port();
    let (listener, bound_port) = match try_bind_with_fallback(primary).await {
        Some(pair) => pair,
        None => {
            tracing::error!(
                target: "sirin",
                "[rpc] Gave up after exhausting ports {primary}..={last}. \
                 Set SIRIN_RPC_PORT=<alt_port> in .env, or run \
                 `Get-Process sirin | Stop-Process -Force` to clear stale binds.",
                last = primary.saturating_add(MAX_PORT_FALLBACK as u16)
            );
            return;
        }
    };

    ACTIVE_PORT.store(bound_port, Ordering::Relaxed);
    RUNNING.store(true, Ordering::Relaxed);
    if bound_port == primary {
        tracing::info!(target: "sirin", "[rpc] Listening on ws://127.0.0.1:{bound_port} + http://127.0.0.1:{bound_port}/mcp");
    } else {
        tracing::warn!(
            target: "sirin",
            "[rpc] Primary port {primary} unavailable — bound fallback port {bound_port} \
             (ws://127.0.0.1:{bound_port} + http://127.0.0.1:{bound_port}/mcp). \
             External MCP clients will need to know about the new port."
        );
    }

    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(target: "sirin", "[rpc] Server error: {e}");
        RUNNING.store(false, Ordering::Relaxed);
        ACTIVE_PORT.store(0, Ordering::Relaxed);
    }
}

/// Maximum number of sequential alternate ports to try after the primary fails.
/// 3 fallbacks (e.g. 7700 → 7701/7702/7703) covers the common multi-zombie case
/// without colliding with arbitrary other services higher up in the range.
const MAX_PORT_FALLBACK: u32 = 3;

/// Attempt to bind `primary`, then `primary+1`, …, up to `MAX_PORT_FALLBACK`
/// alternate ports.  Each port is tried twice with a 1-second backoff in case
/// the previous Sirin's socket is in TCP TIME_WAIT.  Returns `None` if all
/// candidates fail.
async fn try_bind_with_fallback(primary: u16) -> Option<(tokio::net::TcpListener, u16)> {
    for offset in 0..=MAX_PORT_FALLBACK {
        // u16 saturating add — if primary is e.g. u16::MAX, we just stop trying.
        let port = match primary.checked_add(offset as u16) {
            Some(p) => p,
            None => break,
        };
        let addr = format!("127.0.0.1:{port}");
        if let Some(listener) = loop_bind(&addr, 2).await {
            return Some((listener, port));
        }
        if offset < MAX_PORT_FALLBACK {
            tracing::warn!(
                target: "sirin",
                "[rpc] Port {port} unavailable — trying {next} next",
                next = port.saturating_add(1)
            );
        }
    }
    None
}

async fn loop_bind(addr: &str, max_attempts: u32) -> Option<tokio::net::TcpListener> {
    for attempt in 1..=max_attempts {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => return Some(l),
            Err(e) => {
                if attempt == max_attempts {
                    // Suppress final-attempt log here — the caller (try_bind_with_fallback)
                    // logs whether to advance to the next port or give up entirely.
                    tracing::debug!(target: "sirin", "[rpc] Bind {addr} failed (attempt {attempt}/{max_attempts}): {e}");
                    return None;
                }
                tracing::debug!(target: "sirin", "[rpc] Bind {addr} retry (attempt {attempt}/{max_attempts}): {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
    None
}

// ── WebSocket upgrade ─────────────────────────────────────────────────────────

async fn ws_upgrade_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let response = dispatch(text.as_str()).await;
        if socket.send(Message::Text(response.into())).await.is_err() {
            break;
        }
    }
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

async fn dispatch(raw: &str) -> String {
    let req: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("Invalid JSON: {e}") }).to_string(),
    };

    let method = match req.get("method").and_then(Value::as_str) {
        Some(m) => m.to_string(),
        None => return json!({ "error": "Missing 'method' field" }).to_string(),
    };
    let params = req.get("params").cloned().unwrap_or(json!({}));

    let result = match method.as_str() {
        "memory_search" => handle_memory_search(params).await,
        "call_graph_query" => handle_call_graph_query(params).await,
        "trigger_research" => handle_trigger_research(params).await,
        "skill_list" => handle_skill_list().await,
        other => Err(format!("Unknown method: {other}")),
    };

    match result {
        Ok(v) => json!({ "result": v }).to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

// ── Method handlers ───────────────────────────────────────────────────────────

async fn handle_memory_search(params: Value) -> Result<Value, String> {
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .ok_or("Missing 'query'")?
        .to_string();
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;

    tokio::task::spawn_blocking(move || {
        crate::memory::memory_search(&query, limit, "")
            .map(|r| json!(r))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))?
}

async fn handle_call_graph_query(params: Value) -> Result<Value, String> {
    let symbol = params
        .get("symbol")
        .and_then(Value::as_str)
        .ok_or("Missing 'symbol'")?
        .to_string();
    let hops = params.get("hops").and_then(Value::as_u64).unwrap_or(2) as usize;

    tokio::task::spawn_blocking(move || {
        crate::code_graph::query_call_graph(&symbol, hops)
            .map(|r| json!({
                "defined_in": r.defined_in,
                "callers": r.callers,
                "callees": r.callees,
            }))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))?
}

async fn handle_trigger_research(params: Value) -> Result<Value, String> {
    let topic = params
        .get("topic")
        .and_then(Value::as_str)
        .ok_or("Missing 'topic'")?
        .to_string();
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    crate::events::publish(crate::events::AgentEvent::ResearchRequested { topic: topic.clone(), url });
    Ok(json!({ "status": "triggered", "topic": topic }))
}

async fn handle_skill_list() -> Result<Value, String> {
    let skills = crate::skills::list_skills();
    Ok(json!(skills))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_port_defaults_when_env_missing() {
        // Save + clear
        let orig = std::env::var("SIRIN_RPC_PORT").ok();
        std::env::remove_var("SIRIN_RPC_PORT");
        assert_eq!(configured_port(), DEFAULT_RPC_PORT);
        if let Some(v) = orig { std::env::set_var("SIRIN_RPC_PORT", v); }
    }

    #[test]
    fn configured_port_reads_env() {
        let orig = std::env::var("SIRIN_RPC_PORT").ok();
        std::env::set_var("SIRIN_RPC_PORT", "8123");
        assert_eq!(configured_port(), 8123);
        // Invalid falls back to default
        std::env::set_var("SIRIN_RPC_PORT", "not-a-port");
        assert_eq!(configured_port(), DEFAULT_RPC_PORT);
        if let Some(v) = orig { std::env::set_var("SIRIN_RPC_PORT", v); }
        else { std::env::remove_var("SIRIN_RPC_PORT"); }
    }

    /// Verifies the fallback walks up to a free port when the primary is held.
    /// We hold an unrelated socket on `primary`, then ask `try_bind_with_fallback`
    /// to bind starting at that primary — it should advance to `primary+1` (or
    /// further) and return that bound port.
    #[tokio::test]
    async fn try_bind_with_fallback_advances_when_primary_held() {
        // Pick an ephemeral high port the OS gives us, then deliberately
        // attempt to bind starting from there with the listener still alive.
        let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let primary = blocker.local_addr().unwrap().port();

        let result = try_bind_with_fallback(primary).await;
        let (listener, bound) = result.expect("fallback should succeed");
        assert_ne!(bound, primary,
            "expected fallback to advance past held primary {primary}, got {bound}");
        assert!(bound > primary && bound <= primary.saturating_add(MAX_PORT_FALLBACK as u16),
            "bound port {bound} outside fallback window {primary}..={}",
            primary.saturating_add(MAX_PORT_FALLBACK as u16));

        drop(listener);
        drop(blocker);
    }
}
