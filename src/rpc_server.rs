//! Local WebSocket RPC server on `ws://127.0.0.1:7700`.
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

use std::sync::atomic::{AtomicBool, Ordering};

use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use serde_json::{json, Value};

pub const RPC_ADDR: &str = "ws://127.0.0.1:7700";
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Returns `true` once the server has bound and started accepting connections.
pub fn is_running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// Bind the WebSocket server and serve forever. Spawn this as a Tokio task.
pub async fn start_rpc_server() {
    let app = Router::new().route("/", get(ws_upgrade_handler));

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:7700").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[rpc] Failed to bind 127.0.0.1:7700: {e}");
            return;
        }
    };

    eprintln!("[rpc] Listening on {RPC_ADDR}");
    RUNNING.store(true, Ordering::Relaxed);

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("[rpc] Server error: {e}");
        RUNNING.store(false, Ordering::Relaxed);
    }
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
        crate::memory::memory_search(&query, limit)
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
