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
/// Binds to `SIRIN_RPC_PORT` (default 7700).  On transient failure (e.g.
/// recently-killed previous instance in TCP TIME_WAIT), retries up to 3 times
/// with 2-second backoff.  Gives up with a clear diagnostic afterwards.
pub async fn start_rpc_server() {
    let app = Router::new()
        .route("/", get(ws_upgrade_handler))
        .merge(crate::mcp_server::mcp_router());

    let port = configured_port();
    let addr = format!("127.0.0.1:{port}");

    // Retry on transient bind failure — helps when a recently-killed Sirin
    // left the socket in TIME_WAIT / CLOSE_WAIT for a few seconds.
    const MAX_ATTEMPTS: u32 = 3;
    let listener = loop_bind(&addr, MAX_ATTEMPTS).await;
    let listener = match listener {
        Some(l) => l,
        None => {
            eprintln!(
                "[rpc] ⚠️  Gave up binding {addr} after {MAX_ATTEMPTS} attempts. \
                 Set SIRIN_RPC_PORT=<alt_port> in .env to use a different port, \
                 or wait ~2 minutes for Windows TCP TIME_WAIT to clear."
            );
            return;
        }
    };

    ACTIVE_PORT.store(port, Ordering::Relaxed);
    RUNNING.store(true, Ordering::Relaxed);
    eprintln!("[rpc] Listening on ws://{addr} + http://{addr}/mcp");

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("[rpc] Server error: {e}");
        RUNNING.store(false, Ordering::Relaxed);
        ACTIVE_PORT.store(0, Ordering::Relaxed);
    }
}

async fn loop_bind(addr: &str, max_attempts: u32) -> Option<tokio::net::TcpListener> {
    for attempt in 1..=max_attempts {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => return Some(l),
            Err(e) => {
                if attempt == max_attempts {
                    eprintln!("[rpc] Bind {addr} failed on final attempt {attempt}/{max_attempts}: {e}");
                    return None;
                }
                eprintln!("[rpc] Bind {addr} failed (attempt {attempt}/{max_attempts}): {e} — retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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
}
