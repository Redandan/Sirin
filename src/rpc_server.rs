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
/// ## Port selection (Issue #29)
/// Always binds `SIRIN_RPC_PORT` (default 7700) — never falls back to a
/// different port.  If the configured port is held by a stale `sirin.exe`
/// zombie, we taskkill the offender and retry.  If it's held by anything
/// else, we panic with a clear error — silent fallback historically caused
/// `.claude.json` (which hardcodes `127.0.0.1:7700/mcp`) to drift out of
/// sync with the real port, breaking every `mcp__sirin__*` call.
pub async fn start_rpc_server() {
    let app = crate::ext_server::add_ext_routes(
        Router::new()
            .route("/", get(ws_upgrade_handler))
            .merge(crate::mcp_server::mcp_router()),
    );

    let port = configured_port();
    let listener = match bind_with_zombie_kill(port).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(
                target: "sirin",
                "[rpc] Cannot bind 127.0.0.1:{port}: {e}. \
                 Set SIRIN_RPC_PORT=<alt_port> in .env, or free the port manually."
            );
            return;
        }
    };

    ACTIVE_PORT.store(port, Ordering::Relaxed);
    RUNNING.store(true, Ordering::Relaxed);
    tracing::info!(
        target: "sirin",
        "[rpc] Listening on ws://127.0.0.1:{port} + http://127.0.0.1:{port}/mcp"
    );

    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(target: "sirin", "[rpc] Server error: {e}");
        RUNNING.store(false, Ordering::Relaxed);
        ACTIVE_PORT.store(0, Ordering::Relaxed);
    }
}

/// Bind `port` on 127.0.0.1, with one zombie-recovery attempt on failure.
///
/// On Windows: if bind fails, look up the PID holding the port via `netstat`,
/// confirm it's a `sirin.exe` (so we never collateral-kill other software),
/// `taskkill /f` it, sleep 500ms, and retry once.  Any other holder → return
/// the original bind error so the caller can surface a clear panic-style log.
///
/// On non-Windows: just retries once after a short sleep (handles TCP
/// TIME_WAIT churn) without attempting to identify or kill the holder.
async fn bind_with_zombie_kill(port: u16) -> Result<tokio::net::TcpListener, String> {
    let addr = format!("127.0.0.1:{port}");
    let first_err = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => return Ok(l),
        Err(e) => e,
    };

    tracing::warn!(
        target: "sirin",
        "[rpc] Bind {addr} failed ({first_err}); attempting zombie recovery"
    );

    if let Err(e) = kill_zombie_on_port(port) {
        return Err(format!("bind failed ({first_err}); zombie recovery: {e}"));
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind still failing after zombie kill: {e} (initial: {first_err})"))
}

/// Find the PID listening on `127.0.0.1:port` and, if it's a stale `sirin.exe`,
/// `taskkill /f` it.  Returns `Err` if the holder is something else (so we
/// don't silently kill unrelated processes) or if no holder is found.
#[cfg(target_os = "windows")]
fn kill_zombie_on_port(port: u16) -> Result<(), String> {
    use std::process::Command;

    // 1. netstat -ano → parse out PID for the LISTENING line on our port.
    let netstat = Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .map_err(|e| format!("netstat spawn failed: {e}"))?;
    if !netstat.status.success() {
        return Err(format!(
            "netstat exited {}: {}",
            netstat.status,
            String::from_utf8_lossy(&netstat.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&netstat.stdout);
    let needle = format!(":{port}");
    let pid = stdout
        .lines()
        .filter(|l| l.contains("LISTENING"))
        .filter(|l| l.contains(&needle))
        // Local addr is column 2 (after "TCP"); ensure :port is on the local side
        // (i.e. before "LISTENING") and not just somewhere in the line.
        .find_map(|l| {
            let parts: Vec<&str> = l.split_whitespace().collect();
            // Expected: ["TCP", "127.0.0.1:7700", "0.0.0.0:0", "LISTENING", "<pid>"]
            if parts.len() >= 5 && parts[1].ends_with(&needle) {
                parts.last().and_then(|s| s.parse::<u32>().ok())
            } else {
                None
            }
        })
        .ok_or_else(|| format!("no LISTENING entry for :{port} in netstat output"))?;

    // 2. tasklist /fi "pid eq <pid>" /fo csv /nh → check it's sirin.exe.
    let tasklist = Command::new("tasklist")
        .args(["/fi", &format!("pid eq {pid}"), "/fo", "csv", "/nh"])
        .output()
        .map_err(|e| format!("tasklist spawn failed: {e}"))?;
    if !tasklist.status.success() {
        return Err(format!(
            "tasklist exited {}: {}",
            tasklist.status,
            String::from_utf8_lossy(&tasklist.stderr)
        ));
    }
    let row = String::from_utf8_lossy(&tasklist.stdout);
    // CSV row looks like: "sirin.exe","12345","Console","1","42,000 K"
    let proc_name = row
        .trim()
        .split(',')
        .next()
        .map(|s| s.trim_matches('"').to_lowercase())
        .unwrap_or_default();
    if proc_name != "sirin.exe" {
        return Err(format!(
            "port {port} held by PID {pid} ({proc_name}), not sirin.exe — \
             refusing to kill foreign process"
        ));
    }

    // 3. taskkill /pid <pid> /f.
    tracing::warn!(
        target: "sirin",
        "[rpc] Killing zombie sirin.exe PID {pid} holding port {port}"
    );
    let killed = Command::new("taskkill")
        .args(["/pid", &pid.to_string(), "/f"])
        .output()
        .map_err(|e| format!("taskkill spawn failed: {e}"))?;
    if !killed.status.success() {
        return Err(format!(
            "taskkill exited {}: {}",
            killed.status,
            String::from_utf8_lossy(&killed.stderr)
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn kill_zombie_on_port(_port: u16) -> Result<(), String> {
    // Non-Windows path: no taskkill / netstat-CSV plumbing.  The 500ms retry
    // in the caller still lets us recover from TCP TIME_WAIT churn — it just
    // can't blast a stuck process.
    Ok(())
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

    /// `bind_with_zombie_kill` succeeds on a free port without touching
    /// anything else.  This is the happy path — no zombie present.
    #[tokio::test]
    async fn bind_with_zombie_kill_binds_when_port_free() {
        // Get an ephemeral free port, then immediately drop it so it's free
        // (modulo TIME_WAIT, which the retry inside the function tolerates).
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let listener = bind_with_zombie_kill(port)
            .await
            .expect("bind on free port should succeed");
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    /// When a foreign (non-sirin) process is holding the port, we must NOT
    /// kill it — instead `bind_with_zombie_kill` returns Err so the caller
    /// can surface a clear log instead of silently falling back to a
    /// different port (Issue #29).  The test itself plays the role of the
    /// "foreign" holder.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn bind_with_zombie_kill_refuses_to_kill_foreign_holder() {
        // Bind a port and keep the listener alive across the bind attempt.
        // Because cargo-test runs as the test binary (not sirin.exe),
        // `kill_zombie_on_port` should refuse and the outer call should Err.
        let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = blocker.local_addr().unwrap().port();

        let result = bind_with_zombie_kill(port).await;
        assert!(
            result.is_err(),
            "expected Err when port held by non-sirin process, got Ok"
        );

        drop(blocker);
    }
}
