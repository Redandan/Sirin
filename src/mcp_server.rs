//! MCP (Model Context Protocol) HTTP endpoint — `/mcp`
//!
//! 實作 MCP Streamable HTTP transport（2025-03-26 規範）。
//! Claude Desktop 或任何 MCP client 可透過以下設定連接：
//!
//! ```json
//! // claude_desktop_config.json
//! {
//!   "mcpServers": {
//!     "sirin": {
//!       "url": "http://127.0.0.1:7700/mcp"
//!     }
//!   }
//! }
//! ```
//!
//! # 支援的 MCP 方法
//! | 方法 | 說明 |
//! |------|------|
//! | `initialize` | MCP 握手 |
//! | `tools/list` | 列出所有可用工具 |
//! | `tools/call` | 呼叫工具 |
//!
//! # 暴露的工具
//! | 工具名 | 說明 |
//! |--------|------|
//! | `memory_search` | 搜尋 Sirin 記憶庫 |
//! | `skill_list` | 列出所有技能（含 YAML 動態技能）|
//! | `teams_pending` | 取得 Teams 待確認草稿列表 |
//! | `teams_approve` | 核准指定草稿（標記為 Approved，觸發送出）|
//! | `trigger_research` | 觸發研究任務 |
//! | `list_tests` | 列出 `config/tests/` 下所有測試 goal |
//! | `run_test_async` | 非同步觸發測試，立即返回 run_id |
//! | `run_adhoc_test` | 即席測試 — 直接給 URL + goal，不必建 YAML |
//! | `get_test_result` | 依 run_id 取得測試狀態或結果 |
//! | `get_screenshot` | 依 run_id 取得截圖（base64 PNG）|
//! | `get_full_observation` | 取得某步驟的完整（未截斷）observation |
//! | `list_recent_runs` | 查詢歷史測試執行記錄（所有測試或特定 test_id）|
//! | `list_fixes` | 查詢 auto-fix 歷史 |
//! | `config_diagnostics` | 回傳 Sirin 配置診斷（LLM/router/vision 等）|
//! | `browser_exec` | 即席操作瀏覽器（click/type/read/...），不需完整 test goal |

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Json, Path,
    },
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Redirect},
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;

use crate::ui_service::AppService;

// ── Global AppService registry (Phase 2 / web UI) ────────────────────────────
//
// The web UI's /api/snapshot endpoint needs to query agents / runs / coverage,
// all of which live behind the AppService trait. We can't pass the Arc through
// axum State easily since the existing `mcp_router()` is a free function and
// the existing handlers (call_test_coverage / call_run_test_async / …) all
// reach into module-level functions, not handler-local state.
//
// Solution: a `OnceLock<Arc<dyn AppService>>` set by `main.rs` once the
// service is built. Idempotent and lock-free at read time.

static APP_SVC: OnceLock<Arc<dyn AppService>> = OnceLock::new();

/// Called from `main.rs` immediately after `RealService::new()`.
pub fn register_app_service(svc: Arc<dyn AppService>) {
    let _ = APP_SVC.set(svc);
}

fn app_service() -> Option<Arc<dyn AppService>> {
    APP_SVC.get().cloned()
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Max duration any single MCP request may run before the server aborts it.
/// Generous enough for slow `browser_exec` actions (screenshot_analyze → vision
/// LLM ≈ 30–60 s) but short enough to prevent CLOSE_WAIT buildup when a handler
/// hangs on a dead Chrome connection.  Long-running work (run_adhoc_test,
/// run_test_async) already returns immediately with a `run_id`, so they are
/// unaffected.
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

// ── Client ID session store ───────────────────────────────────────────────────
//
// Maps each client's transport identity (HTTP `User-Agent`) to the nice
// client_id derived from its `initialize` params (e.g. "claude-desktop@1.2.3").
// Two concurrent clients with distinct UAs cannot clobber each other — the
// previous `static Mutex<String> CURRENT_CLIENT_ID` had a race where client B's
// `initialize` would overwrite client A's identity mid-flight, leading to
// mis-attributed audit log entries and authz decisions.

static CLIENT_SESSIONS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, String>> {
    CLIENT_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_client_id(user_agent: &str, client_id: &str) {
    sessions().lock().unwrap_or_else(|e| e.into_inner())
        .insert(user_agent.to_string(), client_id.to_string());
}

/// Resolve a request's client_id from its `User-Agent`.  Falls back to the UA
/// itself when the client hasn't yet called `initialize` (e.g. curl probes,
/// sirin-call ad-hoc calls) — better than a stale global or empty string.
fn resolve_client_id(user_agent: &str) -> String {
    sessions().lock().unwrap_or_else(|e| e.into_inner())
        .get(user_agent)
        .cloned()
        .unwrap_or_else(|| user_agent.to_string())
}

// ── Blocking helper with panic recovery ──────────────────────────────────────

/// Run a CPU-bound or blocking-I/O operation on the tokio blocking pool and
/// convert any panic in the closure into an `Err` — NOT a process abort.
///
/// `tokio::task::spawn_blocking` catches panics and reports them via
/// `JoinError::is_panic()`, but only when the binary is built with
/// `panic = "unwind"` (the default — Cargo.toml deliberately leaves `panic`
/// unset in `[profile.release]`).  Under `panic = "abort"` a single bad
/// request would crash the entire Sirin process and leave a zombie listening
/// socket.
async fn blocking<T, F>(label: &'static str, f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(inner) => inner,
        Err(e) if e.is_panic() => {
            tracing::error!(label, "handler panicked — recovered by blocking()");
            Err(format!("{label}: handler panicked — see server logs"))
        }
        Err(e) => Err(format!("{label}: join error: {e}")),
    }
}

// ── UUID short helper ─────────────────────────────────────────────────────────

fn uuid_v4_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", ns)
}

// ── Router ────────────────────────────────────────────────────────────────────

pub fn mcp_router() -> Router {
    // TimeoutLayer enforces a hard deadline on every `/mcp` request.  When it
    // fires, axum drops the handler future and returns 408; the socket is then
    // closed cleanly by hyper (proper FIN) instead of lingering in CLOSE_WAIT
    // while a hung handler (e.g. dead Chrome transport) keeps the connection
    // half-open.  Fixes the zombie-socket pattern observed on ports 7700/7710.
    //
    // CorsLayer allows Claude in Chrome (Beta) extension to issue MCP calls.
    // Browsers send a CORS preflight (OPTIONS) before any cross-origin POST
    // from a `chrome-extension://[id]` page; without this layer the preflight
    // 404s and the extension never gets to send the real request.
    // `GET /gateway` serves an HTML gateway page so Claude in Chrome (Beta) &mdash;
    // whose only network primitive is `navigate` + `javascript_tool` &mdash;
    // can drive the MCP endpoint same-origin (no CORS).  See Issue #90.
    // Path is `/gateway` (not `/`) because `rpc_server::start_rpc_server`
    // already mounts `GET /` as the ext_server WebSocket upgrade route, and
    // axum `merge` panics on overlapping method handlers.
    Router::new()
        .route("/gateway", get(crate::mcp_gateway::gateway_handler))
        .route("/mcp", post(mcp_handler))
        // ── Web UI (Phase 2) ────────────────────────────────────────────
        // Static UI files — bundled into the binary via include_bytes! so
        // the single-binary distribution story holds. /ui (no trailing
        // slash) redirects to /ui/ so relative paths in index.html resolve.
        .route("/ui",        get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui/",       get(ui_index))
        .route("/ui/{file}", get(ui_static))
        // /api/snapshot — single combined JSON read for the dashboard. Polled
        // every ~5s by the web UI; cheap aggregate of read-only AppService calls.
        .route("/api/snapshot", get(api_snapshot))
        // /api/browser_screenshot — raw PNG bytes of the controlled Chrome
        // session. Used by the Browser tab + Dashboard browser card to show
        // a live preview. Returns 503 when no Chrome is open.
        .route("/api/browser_screenshot", get(api_browser_screenshot))
        // /api/chat — Workspace 對話 tab → svc.chat_send(agent_id, message).
        // POST persists user msg + agent reply to chat_history (v0.5.3).
        // GET /api/chat/{agent_id} returns the persisted thread for hydration
        // when the user re-opens the workspace.
        .route("/api/chat",            post(api_chat))
        .route("/api/chat/{agent_id}", get(api_chat_history))
        // ── v0.5.2 mutating endpoints (Workspace 設定 / 待確認 / Settings) ─
        // /api/persona/name — POST { name } updates persona display name.
        .route("/api/persona/name", post(api_persona_set_name))
        // /api/agent/{id} — GET full AgentDetailView; POST dispatches an
        // action via { action, ...args }: toggle / behavior / objective_add
        // / objective_remove. Single endpoint instead of 4 keeps wiring
        // tidy — match in Rust is cheaper than 4 axum routes.
        .route("/api/agent/{id}", get(api_agent_get).post(api_agent_action))
        // /api/pending/{agent_id} — GET list of draft replies waiting on
        // human review. POST { reply_id, action: approve|reject|edit, text? }.
        .route("/api/pending/{agent_id}", get(api_pending_list).post(api_pending_action))
        // ── v0.5.4: WebSocket push channel ──────────────────────────────
        // /ws — long-lived connection that streams snapshot JSON every 2s.
        // Replaces the 5s HTTP poll for connected clients (each open tab
        // gets its own ticker). UI silently falls back to polling on close.
        .route("/ws", get(ws_handler))
        // On-demand modal endpoints (split out from /api/snapshot in
        // v0.5.4 — these block on slow ops like config_check probing
        // lmstudio over the network).
        .route("/api/health",         get(api_health))
        .route("/api/logs",           get(api_logs))
        .route("/api/team_dashboard", get(api_team_dashboard))
        .layer(cors_layer())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            MCP_REQUEST_TIMEOUT,
        ))
}

// ── Web UI handlers ──────────────────────────────────────────────────────────

async fn ui_index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../web/index.html"),
    )
}

async fn ui_static(Path(file): Path<String>) -> impl IntoResponse {
    // Each branch returns include_bytes! at compile time — no filesystem I/O,
    // no path-traversal risk. Adding a new file is a one-line patch here.
    let (mime, body): (&'static str, &'static [u8]) = match file.as_str() {
        "index.html"    => ("text/html; charset=utf-8",    include_bytes!("../web/index.html")),
        "style.css"     => ("text/css; charset=utf-8",     include_bytes!("../web/style.css")),
        "app.js"        => ("application/javascript",      include_bytes!("../web/app.js")),
        "alpine.min.js" => ("application/javascript",      include_bytes!("../web/alpine.min.js")),
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    ([(header::CONTENT_TYPE, mime)], body).into_response()
}

/// Workspace 對話 tab → `chat_send` proxy + persists both user message
/// and agent reply to chat_history. Synchronous wait while LLM responds.
async fn api_chat(Json(body): Json<Value>) -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error": "service unavailable"}))).into_response(),
    };
    let agent_id = body.get("agent_id").and_then(Value::as_str).unwrap_or("").to_string();
    let message  = body.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    if agent_id.is_empty() || message.is_empty() {
        return (StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "agent_id and message required"}))).into_response();
    }

    // Persist the user message immediately so it survives even if chat_send
    // panics or the daemon dies mid-LLM-call.
    let _ = crate::chat_history::append(&agent_id, "user", &message);

    let agent_id_for_task = agent_id.clone();
    let message_for_task  = message.clone();
    let reply = tokio::task::spawn_blocking(move || {
        svc.chat_send(&agent_id_for_task, &message_for_task)
    }).await;
    match reply {
        Ok(r) => {
            let _ = crate::chat_history::append(&agent_id, "agent", &r);
            axum::Json(json!({ "reply": r })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": format!("chat task: {e}")}))).into_response(),
    }
}

/// GET /api/chat/{agent_id} → array of {role, text, created_at} oldest first.
async fn api_chat_history(Path(agent_id): Path<String>) -> impl IntoResponse {
    match crate::chat_history::history(&agent_id, 200) {
        Ok(msgs) => axum::Json(msgs.into_iter().map(|m| json!({
            "role":       m.role,
            "text":       m.text,
            "created_at": m.created_at,
        })).collect::<Vec<_>>()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e}))).into_response(),
    }
}

/// POST /api/persona/name { name } → 200 OK.
async fn api_persona_set_name(Json(body): Json<Value>) -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":"service unavailable"}))).into_response(),
    };
    let name = body.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":"name required"}))).into_response();
    }
    tokio::task::spawn_blocking(move || svc.set_persona_name(&name)).await.ok();
    axum::Json(json!({"ok": true})).into_response()
}

/// GET /api/agent/{id} → AgentDetailView JSON.
async fn api_agent_get(Path(id): Path<String>) -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "service unavailable").into_response(),
    };
    let id_owned = id.clone();
    let detail = tokio::task::spawn_blocking(move || svc.agent_detail(&id_owned)).await.ok().flatten();
    match detail {
        Some(d) => axum::Json(d).into_response(),
        None    => (StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("agent {id} not found")}))).into_response(),
    }
}

/// POST /api/agent/{id} { action, ... } — dispatches:
///   • toggle           { enabled: bool }
///   • behavior         { enabled, min_delay, max_delay, max_hour, max_day }
///   • objective_add    { text }
///   • objective_remove { index }
///   • set_remote_ai    { allowed: bool }
async fn api_agent_action(Path(id): Path<String>, Json(body): Json<Value>) -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":"service unavailable"}))).into_response(),
    };
    let action = body.get("action").and_then(Value::as_str).unwrap_or("").to_string();
    let id_owned = id.clone();
    let body_owned = body.clone();

    let res: Result<(), String> = tokio::task::spawn_blocking(move || -> Result<(), String> {
        match action.as_str() {
            "toggle" => {
                let enabled = body_owned.get("enabled").and_then(Value::as_bool)
                    .ok_or("enabled required")?;
                svc.toggle_agent(&id_owned, enabled);
                Ok(())
            }
            "behavior" => {
                let enabled    = body_owned.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                let min_delay  = body_owned.get("min_delay").and_then(Value::as_u64).unwrap_or(3);
                let max_delay  = body_owned.get("max_delay").and_then(Value::as_u64).unwrap_or(15);
                let max_hour   = body_owned.get("max_hour").and_then(Value::as_u64).unwrap_or(30) as u32;
                let max_day    = body_owned.get("max_day").and_then(Value::as_u64).unwrap_or(200) as u32;
                svc.set_behavior(&id_owned, enabled, min_delay, max_delay, max_hour, max_day);
                Ok(())
            }
            "objective_add" => {
                let text = body_owned.get("text").and_then(Value::as_str)
                    .ok_or("text required")?.to_string();
                svc.add_objective(&id_owned, &text);
                Ok(())
            }
            "objective_remove" => {
                let index = body_owned.get("index").and_then(Value::as_u64)
                    .ok_or("index required")? as usize;
                svc.remove_objective(&id_owned, index);
                Ok(())
            }
            "set_remote_ai" => {
                let allowed = body_owned.get("allowed").and_then(Value::as_bool)
                    .ok_or("allowed required")?;
                svc.set_remote_ai(&id_owned, allowed);
                Ok(())
            }
            other => Err(format!("unknown action '{other}'")),
        }
    }).await.unwrap_or_else(|e| Err(format!("task: {e}")));

    match res {
        Ok(())  => axum::Json(json!({"ok": true})).into_response(),
        Err(e)  => (StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": e}))).into_response(),
    }
}

/// GET /api/pending/{agent_id} → Vec<PendingReplyView>.
async fn api_pending_list(Path(agent_id): Path<String>) -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "service unavailable").into_response(),
    };
    let pending = tokio::task::spawn_blocking(move || svc.load_pending(&agent_id))
        .await.unwrap_or_default();
    axum::Json(pending).into_response()
}

/// POST /api/pending/{agent_id} { reply_id, action: approve|reject|edit, text? }.
async fn api_pending_action(
    Path(agent_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":"service unavailable"}))).into_response(),
    };
    let reply_id = body.get("reply_id").and_then(Value::as_str).unwrap_or("").to_string();
    let action   = body.get("action").and_then(Value::as_str).unwrap_or("").to_string();
    let text     = body.get("text").and_then(Value::as_str).map(String::from);
    if reply_id.is_empty() || action.is_empty() {
        return (StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":"reply_id and action required"}))).into_response();
    }
    let res: Result<(), String> = tokio::task::spawn_blocking(move || -> Result<(), String> {
        match action.as_str() {
            "approve" => { svc.approve_reply(&agent_id, &reply_id); Ok(()) }
            "reject"  => { svc.reject_reply(&agent_id, &reply_id);  Ok(()) }
            "edit"    => {
                let t = text.ok_or("text required for edit")?;
                svc.edit_draft(&agent_id, &reply_id, &t);
                Ok(())
            }
            other => Err(format!("unknown action '{other}'")),
        }
    }).await.unwrap_or_else(|e| Err(format!("task: {e}")));

    match res {
        Ok(())  => axum::Json(json!({"ok": true})).into_response(),
        Err(e)  => (StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": e}))).into_response(),
    }
}

// ── v0.5.4 on-demand modal endpoints ────────────────────────────────
// These are slower operations (network probes, log buffer scan, worker
// state aggregation) that we don't want in the 2s WebSocket push tick.
// UI fetches them once when the corresponding modal opens.

/// GET /api/health — config_check + persona + LLM info for Settings modal.
async fn api_health() -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":"service unavailable"}))).into_response(),
    };
    let res = tokio::task::spawn_blocking(move || -> Value {
        let issues_raw = svc.config_check();
        let ok_count = issues_raw.iter()
            .filter(|i| matches!(i.severity, crate::ui_service::ConfigSeverity::Ok))
            .count();
        let issues: Vec<Value> = issues_raw.into_iter()
            .filter(|i| !matches!(i.severity, crate::ui_service::ConfigSeverity::Ok))
            .map(|i| json!({
                "severity":   match i.severity {
                    crate::ui_service::ConfigSeverity::Error   => "error",
                    crate::ui_service::ConfigSeverity::Warning => "warning",
                    crate::ui_service::ConfigSeverity::Info    => "info",
                    crate::ui_service::ConfigSeverity::Ok      => "ok",
                },
                "category":   i.category,
                "message":    i.message,
                "suggestion": i.suggestion,
            })).collect();
        json!({
            "config_issues":   issues,
            "config_ok_count": ok_count,
        })
    }).await;
    match res {
        Ok(v)  => axum::Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": format!("health task: {e}")}))).into_response(),
    }
}

/// GET /api/logs — last 50 log lines for Logs modal.
async fn api_logs() -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":"service unavailable"}))).into_response(),
    };
    let res = tokio::task::spawn_blocking(move || -> Vec<Value> {
        svc.log_recent(50).into_iter().map(|l| {
            let level = match l.level {
                crate::ui_service::LogLevel::Error    => "error",
                crate::ui_service::LogLevel::Warn     => "warn",
                _                                     => "info",
            };
            json!({ "level": level, "text": l.text, "ts": "" })
        }).collect()
    }).await;
    match res {
        Ok(v)  => axum::Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": format!("logs task: {e}")}))).into_response(),
    }
}

/// GET /api/team_dashboard — Dev Squad live state + token burn.
async fn api_team_dashboard() -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":"service unavailable"}))).into_response(),
    };
    let res = tokio::task::spawn_blocking(move || -> Value {
        let dash = svc.team_dashboard();
        let usage = svc.team_token_usage(300);
        json!({
            "team_dashboard": {
                "worker_running": dash.worker_running,
                "queued":  dash.queued,
                "running": dash.running,
                "done":    dash.done,
                "failed":  dash.failed,
                "pm":        member_view(&dash.pm),
                "engineer":  member_view(&dash.engineer),
                "tester":    member_view(&dash.tester),
            },
            "token_usage": {
                "window_secs":    usage.window_secs,
                "api_calls":      usage.api_calls,
                "tokens_per_min": usage.tokens_per_min,
                "cost_per_hour":  usage.cost_per_hour,
                "cache_hit_pct":  usage.cache_hit_pct,
            },
        })
    }).await;
    match res {
        Ok(v)  => axum::Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": format!("team task: {e}")}))).into_response(),
    }
}

// ── WebSocket push (v0.5.4) ──────────────────────────────────────────
//
// Each connected client gets its own task with a 2-second ticker. We push
// the same JSON the /api/snapshot endpoint returns — the web UI parses it
// and replaces its state, identical to the HTTP fallback path.
//
// Tradeoffs vs server-sent events / diffs:
//   • Bidirectional WS lets the client send acks / requests later (e.g.
//     subscribe to a specific log severity) — future-proof.
//   • Pushing the full snapshot is wasteful (~30 KB JSON) but trivial to
//     reason about and totally fine for a single-machine localhost daemon.
//   • Diff'ing against last sent state is a follow-up if bandwidth ever
//     matters (it doesn't, here).

async fn ws_handler(upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(handle_ws)
}

async fn handle_ws(mut socket: WebSocket) {
    use std::time::Duration;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    // Skip the first immediate tick — we send a snapshot inline below to
    // hydrate the UI as fast as possible after connect.
    tick.tick().await;

    // Initial push.
    if !push_snapshot(&mut socket).await { return; }

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !push_snapshot(&mut socket).await { return; }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None      => return,
                Some(Ok(Message::Ping(p)))              => {
                    let _ = socket.send(Message::Pong(p)).await;
                }
                _ => {} // ignore other message types
            },
        }
    }
}

/// Build a snapshot off the runtime + send as a text frame. Returns true
/// when the send succeeded; false ends the loop.
async fn push_snapshot(socket: &mut WebSocket) -> bool {
    let svc = match app_service() {
        Some(s) => s,
        None    => return true, // service not yet registered — keep waiting
    };
    let snap = tokio::task::spawn_blocking(move || build_snapshot(&svc)).await;
    let body = match snap {
        Ok(v)  => v.to_string(),
        Err(_) => return true, // task join error — skip this tick
    };
    socket.send(Message::Text(body.into())).await.is_ok()
}

/// PNG bytes of the controlled Chrome session. Used by the Browser tab
/// and Dashboard browser card. Returns 503 (with image/png placeholder
/// arguably nicer, but plain text is fine) when no browser is open.
async fn api_browser_screenshot() -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "AppService not registered").into_response(),
    };
    let png = tokio::task::spawn_blocking(move || svc.browser_screenshot()).await;
    match png {
        Ok(Some(bytes)) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-cache, max-age=0"),
            ],
            bytes,
        ).into_response(),
        Ok(None)  => (StatusCode::SERVICE_UNAVAILABLE, "browser not open").into_response(),
        Err(e)    => (StatusCode::INTERNAL_SERVER_ERROR, format!("screenshot task: {e}")).into_response(),
    }
}

/// Single combined snapshot for the dashboard's 5-second poll. Aggregates
/// read-only AppService calls; failures degrade to default values rather
/// than 500 so the UI keeps rendering with a partial state.
async fn api_snapshot() -> impl IntoResponse {
    let svc = match app_service() {
        Some(s) => s,
        None => return axum::Json(json!({
            "error": "AppService not registered (sirin still booting?)"
        })).into_response(),
    };

    // Run inside spawn_blocking — most AppService methods do synchronous
    // SQLite reads + parser work; we don't want to block the async runtime.
    let snap = tokio::task::spawn_blocking(move || build_snapshot(&svc)).await;
    match snap {
        Ok(v)  => axum::Json(v).into_response(),
        Err(e) => axum::Json(json!({
            "error": format!("snapshot task failed: {e}")
        })).into_response(),
    }
}

fn build_snapshot(svc: &Arc<dyn AppService>) -> Value {
    let s = svc.system_status();

    let agents: Vec<Value> = svc.list_agents().into_iter().map(|a| json!({
        "id":          a.id,
        "name":        a.name,
        "platform":    a.platform,
        "live_status": a.live_status,
        "enabled":     a.enabled,
    })).collect();

    let mut pending = serde_json::Map::new();
    for a in svc.list_agents() {
        pending.insert(a.id.clone(), json!(svc.pending_count(&a.id)));
    }

    let active_runs: Vec<Value> = svc.active_test_runs().into_iter().map(|r| json!({
        "test_id":   r.test_id,
        "status":    r.status,
        "step":      r.step,
        "analysis":  r.analysis,
        "started_at": r.started_at,
        // #279 click-to-expand: run_id needed by frontend to fetch
        // get_test_result / live_trace via /mcp.
        "run_id":    r.run_id,
        "is_replay": r.is_replay,
    })).collect();

    let recent_runs: Vec<Value> = svc.recent_test_runs(8).into_iter().map(|r| json!({
        "test_id":     r.test_id,
        "status":      r.status,
        "duration_ms": r.duration_ms,
        "started_at":  r.started_at,
        "pass_rate":   r.pass_rate,
        "failure_category": r.failure_category,
        // Issue #240: token + cost telemetry surfaced to the dashboard.
        // null for old rows / replays / runs with no LLM traffic.
        "prompt_tokens":     r.prompt_tokens,
        "completion_tokens": r.completion_tokens,
        "cached_tokens":     r.cached_tokens,
        "cost_usd":          r.cost_usd,
        // #279 click-to-expand: surface iter count + replay mode + run_id
        // so the dashboard row click can fetch step history.
        "iterations":  r.iterations,
        "is_replay":   r.is_replay,
        "run_id":      r.run_id,
        "analysis":    r.analysis,  // short final_analysis for inline preview
    })).collect();

    let last_verdict = svc.recent_test_runs(1).into_iter().next().map(|r| json!({
        "test_id":   r.test_id,
        "status":    r.status,
    }));

    let coverage = svc.test_coverage_data().ok().map(|c| json!({
        "product":        c.product,
        "version":        c.version,
        "total_features": c.total_features,
        "total_covered":  c.total_covered,
        "scripted":       c.scripted,
        "discovered":     c.discovered,
        "discovery_status": match c.discovery_status {
            crate::ui_service::DiscoveryStatus::NotRun     => "NotRun",
            crate::ui_service::DiscoveryStatus::Crawling { .. } => "Crawling",
            crate::ui_service::DiscoveryStatus::Done { .. }     => "Done",
        },
        "groups":         c.groups.into_iter().map(|g| json!({
            "id":       g.id,
            "name":     g.name,
            "role":     g.role,
            "covered":  g.covered,
            "total":    g.total,
            "features": g.features.into_iter().map(|f| json!({
                "id":       f.id,
                "name":     f.name,
                "status":   f.status,
                "test_ids": f.test_ids,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    }));

    // #266: replay-success-rate KPI for Dashboard + per-test status for Coverage.
    // Window=14d so the metric splits into "last 7d" + "prior 7d" for drift.
    let replay_health = svc.replay_health(14);
    let replay_health_json = json!({
        "window_days":              replay_health.window_days,
        "total_runs":               replay_health.total_runs,
        "passed_via_replay":        replay_health.passed_via_replay,
        "passed_via_llm":           replay_health.passed_via_llm,
        "failed":                   replay_health.failed,
        "success_rate_replay":      replay_health.success_rate_replay,
        "previous_success_rate_replay": replay_health.previous_success_rate_replay,
        "drift":                    replay_health.drift,
    });
    let test_replay_statuses: Vec<Value> = svc.test_replay_statuses().into_iter().map(|t| json!({
        "test_id":     t.test_id,
        "status":      t.status,
        "has_script":  t.has_script,
        "recent_modes": t.recent_modes,
    })).collect();

    json!({
        "version":        env!("CARGO_PKG_VERSION"),
        "browser_open":   svc.browser_is_open(),
        "browser_url":    svc.browser_url(),
        "browser_title":  svc.browser_title(),
        "rpc_running":    s.rpc_running,
        "tg_connected":   s.telegram_connected,
        "last_verdict":   last_verdict,
        "agents":         agents,
        "pending_counts": pending,
        "active_runs":    active_runs,
        "recent_runs":    recent_runs,
        "coverage":       coverage,
        "replay_health":         replay_health_json,
        "test_replay_statuses":  test_replay_statuses,
        // Cheap fields kept inline:
        "persona_name":   svc.persona_name(),
        "llm_main":       s.llm.main_model,
        "llm_router":     s.llm.router_model,
        // v0.5.4: heavyweight fields (config_check probes lmstudio over the
        // network; team_token_usage scans worker logs) moved to dedicated
        // /api/health, /api/logs, /api/team_dashboard endpoints fetched
        // only when the corresponding modal opens. Snapshot stays fast
        // (~50 ms) so 2 s WebSocket push is sustainable.
    })
}

/// Flatten a `TeamMemberView` for snapshot JSON. Web UI's Dev Squad modal
/// expects { role, session_id, turns } — we drop `resume_cmd` since the
/// browser shouldn't expose CLI session commands.
fn member_view(m: &crate::ui_service::TeamMemberView) -> Value {
    json!({
        "role":       m.role,
        "session_id": m.session_id,
        "turns":      m.turns,
    })
}

/// Build the CorsLayer governing `/mcp`.
///
/// When `CLAUDE_CHROME_EXT_ID` is set, only `chrome-extension://<id>` is
/// allowed — this is the strict mode for Claude in Chrome (Beta).  The
/// extension ID is fixed by Chrome and the browser refuses to forge the
/// `Origin` header, so this is a hard authentication boundary against any
/// other extension or web origin.
///
/// When the env var is unset, all origins are allowed for backward
/// compatibility with Claude Desktop, sirin-call, curl probes, and other
/// non-browser MCP clients that don't send an `Origin` header at all.
fn cors_layer() -> CorsLayer {
    let base = CorsLayer::new()
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers(Any);
    match std::env::var("CLAUDE_CHROME_EXT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(id) => {
            let origin = format!("chrome-extension://{}", id.trim());
            match origin.parse() {
                Ok(hv) => base.allow_origin(AllowOrigin::exact(hv)),
                Err(e) => {
                    tracing::warn!(
                        "CLAUDE_CHROME_EXT_ID={id:?} not a valid origin ({e}); falling back to allow-any"
                    );
                    base.allow_origin(Any)
                }
            }
        }
        None => base.allow_origin(Any),
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

async fn mcp_handler(
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> impl IntoResponse {
    let id     = req.get("id").cloned().unwrap_or(json!(null));
    let method = req["method"].as_str().unwrap_or("").to_string();
    let params = req.get("params").cloned().unwrap_or(json!({}));
    // Transport-level client identity — stable across tools/call requests for
    // the same HTTP client (same User-Agent).  Feeds resolve_client_id so
    // audit logs and authz decisions use the right identity instead of a
    // last-writer-wins global.
    let user_agent = headers.get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let result = dispatch(&method, params, &user_agent).await;

    let body = match result {
        Ok(v) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": v,
        }),
        Err(msg) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": msg },
        }),
    };

    axum::Json(body)
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

async fn dispatch(method: &str, params: Value, user_agent: &str) -> Result<Value, String> {
    match method {
        "initialize" => handle_initialize(params, user_agent),
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tools_call(params, user_agent).await,
        // Notifications (no response required, but we must not error)
        "notifications/initialized" => Ok(json!({})),
        other => Err(format!("Method not found: {other}")),
    }
}

// ── initialize ────────────────────────────────────────────────────────────────

fn handle_initialize(params: Value, user_agent: &str) -> Result<Value, String> {
    // Parse clientInfo → "name@version", fallback "unknown@unknown"
    let name    = params["clientInfo"]["name"].as_str().unwrap_or("unknown");
    let version = params["clientInfo"]["version"].as_str().unwrap_or("unknown");
    let client_id = format!("{name}@{version}");
    // Remember this nice id for the duration of the client's session (keyed
    // by User-Agent, so concurrent clients with different UAs don't collide).
    remember_client_id(user_agent, &client_id);
    // Register with monitor
    if let Some(state) = crate::monitor::state() {
        state.mark_client(&client_id, true);
    }
    Ok(json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name":    "sirin",
            "version": env!("CARGO_PKG_VERSION"),
        }
    }))
}

// ── tools/list ────────────────────────────────────────────────────────────────

fn handle_tools_list() -> Result<Value, String> {
    let mut response = handle_tools_list_legacy()?;
    // Issue #257 Option B — append registry-defined tools.  Migrated tools
    // live in `mcp_registry::seed_default`; un-migrated tools stay in the
    // literal block below.  Both sets are guarded by the parity test in
    // `mcp_parity_tests` to ensure neither path drifts out of sync.
    if let Some(tools_arr) = response["tools"].as_array_mut() {
        for def in crate::mcp_registry::iter() {
            tools_arr.push(def.to_json());
        }
    }
    Ok(response)
}

/// Legacy literal block — un-migrated tools.  Each entry here will be
/// deleted as it gets a `register_tool` call in `mcp_registry::seed_default`.
/// See #257 Option B migration tracker.
fn handle_tools_list_legacy() -> Result<Value, String> {
    Ok(json!({
        "tools": [
            // #257 — text-mode tools (memory_search / teams_approve / trigger_research) →
            //         mcp_registry::seed_default (uses SyncText / AsyncText variants).
            //         skill_list / teams_pending also already there (batch 1).
            // #257 — entire test_runner sync subset → mcp_registry::seed_default:
            //   reads: list_tests, get_test_result, get_screenshot, get_full_observation,
            //          get_run_trace, list_recent_runs, test_analytics, test_summary,
            //          list_flaky_tests, test_coverage, script_health, replay_last_failure,
            //          shadow_dump_diff, compare_with_replay
            //   actions: run_test_async, run_test_batch, run_test_pipeline, run_adhoc_test,
            //            persist_adhoc_run, kill_run, scaffold_test, lint_test, discover_app,
            //            discovery_features (+ already-migrated discovery_status)
            // #257 — list_recent_runs / test_summary / test_analytics / test_coverage → mcp_registry::seed_default
            // #257 — replay scripts (list_saved_scripts, delete_saved_script) → mcp_registry::seed_default
            // (ui_navigate / ui_state removed in Phase 7 — UI is now a web
            //  app; AI drives it via /mcp + browser_exec on the same Chrome.)
            // #257 — list_flaky_tests → mcp_registry::seed_default
            // #257 — script_health / replay_last_failure / shadow_dump_diff / compare_with_replay → mcp_registry::seed_default
            // #257 — auto-fix history (list_fixes) → mcp_registry::seed_default
            // #257 — #225 claude-config-mcp full sync subset → mcp_registry::seed_default
            //         (list_allowlist, list_slash_commands, list_hooks, list_redundant_allow,
            //          suggest_allowlist, add_allow, remove_allow)
            // #257 — #227 session-memory-mcp (save/list/restore/expire_point) all in mcp_registry::seed_default
            // #257 — #232 session-cost-mcp (session_cost, list_expensive_sessions) → mcp_registry::seed_default
            // #257 — #228 task-tracker-mcp (create_task/list_tasks/mark_task_done/link_task) all in mcp_registry::seed_default
            // #257 — #224 handoff-mcp (create/get_latest/list_handoff_history) all in mcp_registry::seed_default
            // #257 — async cluster (16 tools) → mcp_registry::seed_default:
            //   explain_failure, kb_stats/diff/duplicate_check/merge,
            //   generate_daily_brief, route_query, query_llm, fallback_chain,
            //   benchmark_llms, page_state, run_regression_suite, sirin_preflight,
            //   assistant_task, kb_search, kb_get, kb_write
            // #257 — #233 sync subset (list_intents, register_intent) → mcp_registry::seed_default
            // #257 — config_diagnostics / diagnose moved to mcp_registry::seed_default
            {
                "name": "browser_exec",
                "description": "即席執行瀏覽器動作，不走完整 test goal。適合 debug / 探索 / 單步操作。\n\n基本導航：goto, screenshot, screenshot_analyze, click, click_point, type, read, eval, wait, exists, attr, scroll, key, console, network, url, title, close, set_viewport\n\nAX tree（literal text，精確比對）：enable_a11y, ax_tree, ax_find（支援 scroll / scroll_max / name_regex / not_name_matches / limit）, ax_value, ax_click, ax_focus, ax_type, ax_type_verified\n\nAX snapshots：ax_snapshot, ax_diff, wait_for_ax_change\n\nCondition waits（取代 sleep）：wait_for_url, wait_for_ax_ready, wait_for_network_idle\n\nAssertions：assert_ax_contains, assert_url_matches\n\nMulti-session（跨角色 E2E）：list_sessions, close_session；所有動作可加 session_id 參數切換 tab\n\nTest isolation / popup：clear_state, wait_new_tab, wait_request\n\nFlutter CanvasKit（Shadow DOM，用於 WebGL canvas 應用）：先呼叫 enable_a11y 觸發 flt-semantics-host，再用以下動作 — shadow_dump（列出所有元素）, shadow_find（找元素，params: role/name_regex）, shadow_click（JS PointerEvent 點擊，比 click 可靠）, shadow_type（focus+InsertText，非 Flutter 用）, shadow_type_flutter（shadow_click+flutter_type 合一）, flutter_type（逐字 keydown，ASCII only，不支援中文）, flutter_enter（對 flt-text-editing 發 Enter，用於送出訊息/表單）",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action":             { "type": "string", "description": "browser action 名稱" },
                        "target":             { "type": "string", "description": "URL / CSS selector / JS expr / 文字，視 action 而定。close_session 時為 session_id" },
                        "text":               { "type": "string", "description": "type / ax_type 動作的輸入文字" },
                        "timeout":            { "type": "number", "description": "wait* 動作的 ms timeout（wait_for_url/ax_ready 預設 10000，wait_for_network_idle 預設 15000）" },
                        "x":                  { "type": "number", "description": "click_point 的 x 座標（單位由 coord_source 決定，預設 CSS px）" },
                        "y":                  { "type": "number", "description": "click_point 的 y 座標（單位由 coord_source 決定，預設 CSS px）" },
                        "coord_source":       { "type": "string", "enum": ["css", "screenshot"], "description": "click_point/hover_point 的座標單位：'css'（預設，CDP 直用）或 'screenshot'（截圖物理像素，會自動除以 devicePixelRatio 修正 HiDPI 偏移）" },
                        "width":              { "type": "number", "description": "set_viewport 的 width (px)" },
                        "height":             { "type": "number", "description": "set_viewport 的 height (px)" },
                        "device_scale":       { "type": "number", "description": "set_viewport 的 devicePixelRatio (預設 1.0)" },
                        "mobile":             { "type": "boolean", "description": "set_viewport 的 mobile 模擬旗標 (預設 false)" },
                        "browser_headless":   { "type": "boolean", "description": "Flutter/WebGL 應該設 false。預設讀 SIRIN_BROWSER_HEADLESS env" },
                        "backend_id":         { "type": "number", "description": "ax_value/ax_click/ax_focus/ax_type 的 DOM backend node id (從 ax_tree / ax_find 取得)" },
                        "role":               { "type": "string", "description": "ax_find 的 a11y role 過濾 (e.g. button, textbox, text)" },
                        "name":               { "type": "string", "description": "ax_find 的 name 子字串過濾 (case-insensitive)" },
                        "name_regex":         { "type": "string", "description": "ax_find: Rust regex 對 name 全文比對（比 name 子字串更精確）" },
                        "not_name_matches":   { "type": "array", "items": { "type": "string" }, "description": "ax_find: 排除 name 包含任一字串的節點" },
                        "limit":              { "type": "number", "description": "ax_find: >1 時返回 nodes 陣列（多節點），=1 時返回單節點（預設 1）" },
                        "scroll":             { "type": "boolean", "description": "ax_find: true 時自動往下捲動直到找到元素（Flutter ListView / 分頁）" },
                        "scroll_max":         { "type": "number", "description": "ax_find scroll 最多捲幾次（預設 10，每次 400px）" },
                        "include_ignored":    { "type": "boolean", "description": "ax_tree 是否包含 ignored / generic 節點 (預設 false)" },
                        "id":                 { "type": "string", "description": "ax_snapshot: 自訂快照 ID（省略則自動生成）" },
                        "before_id":          { "type": "string", "description": "ax_diff: 前一個快照 ID" },
                        "after_id":           { "type": "string", "description": "ax_diff: 後一個快照 ID" },
                        "baseline_id":        { "type": "string", "description": "wait_for_ax_change: 基準快照 ID" },
                        "min_nodes":          { "type": "number", "description": "wait_for_ax_ready: AX tree 最少需要幾個節點（預設 20）" },
                        "idle_ms":            { "type": "number", "description": "wait_for_network_idle: 網路安靜多久算完成（預設 500ms）" },
                        "session_id":         { "type": "string", "description": "多 session 支援：每個 session_id 對應一個獨立 tab（e.g. buyer_a / buyer_b）。省略時使用當前 active tab" }
                    },
                    "required": ["action"]
                }
            },
            // #257 — sync agent / dev_team / cross-session helpers all in mcp_registry::seed_default:
            //   consult, supervised_run, agent_team_status/task/test,
            //   agent_send, agent_reset, agent_enqueue, agent_start_worker,
            //   agent_queue_status, agent_clear_completed, squad_knowledge,
            //   dev_team_enqueue_issue/list_previews/replay_preview/read_issue
            // #257 — assistant_task / kb_search / kb_get / kb_write moved to
            //         mcp_registry::seed_default (async batch)
            // #257 — browser_status / sync_config moved to mcp_registry::seed_default
            // #257 — run_regression_suite / sirin_preflight moved to
            //         mcp_registry::seed_default (async batch)
        ]
    }))
}

// ── tools/call ────────────────────────────────────────────────────────────────

async fn handle_tools_call(params: Value, user_agent: &str) -> Result<Value, String> {
    let name      = params["name"].as_str().ok_or("Missing 'name'")?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    // Issue #257 Option B — registry path FIRST. Migrated tools are
    // dispatched here; un-migrated tools fall through to the legacy
    // match arms below.  See `crate::mcp_registry::seed_default` for the
    // running list of migrated tools.
    if let Some(def) = crate::mcp_registry::get(name) {
        return def.handler.invoke(arguments).await;
    }

    // Tools that return structured JSON (not just text) bypass the text wrapper.
    // Only `browser_exec` currently needs the caller's identity (for authz +
    // audit); other tools are read-only w.r.t. authz.
    match name {
        // #257 — entire test_runner sync subset → mcp_registry::seed_default:
        //   reads: list_tests, get_test_result, get_screenshot, get_full_observation,
        //          get_run_trace, list_recent_runs, test_analytics, test_summary,
        //          list_flaky_tests, test_coverage, script_health, replay_last_failure,
        //          shadow_dump_diff, compare_with_replay
        //   actions: run_test_async, run_test_batch, run_test_pipeline, run_adhoc_test,
        //            persist_adhoc_run, kill_run, scaffold_test, lint_test, discover_app,
        //            discovery_features (+ already-migrated discovery_status)
        // ui_navigate / ui_state — removed with the egui shell in Phase 7.
        // #257 — at this point every JSON tool except browser_exec lives in
        //         mcp_registry::seed_default.  browser_exec is the sole
        //         legacy-path holdout (see CLAUDE.md — needs user_agent
        //         for authz, not worth widening the registry signature
        //         to accommodate one tool).
        "browser_exec"         => return call_browser_exec(arguments, user_agent).await.map(wrap_json),
        _ => return Err(format!("Unknown tool: {name}")),
    }
}

/// Wrap arbitrary JSON payload as MCP content blocks.
fn wrap_json(payload: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
        }]
    })
}

// ── Tool implementations ──────────────────────────────────────────────────────

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_memory_search(args: Value) -> Result<String, String> {
    let query = args["query"].as_str().ok_or("Missing query")?.to_string();
    let limit = args["limit"].as_u64().unwrap_or(5) as usize;

    blocking("memory_search", move || {
        crate::memory::memory_search(&query, limit, "")
            .map(|results| results.join("\n\n"))
            .map_err(|e| e.to_string())
    })
    .await
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_skill_list() -> String {
    let skills = crate::skills::list_skills();
    skills
        .iter()
        .map(|s| format!("• **{}** ({})\n  {}", s.name, s.category, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_teams_pending() -> String {
    let pending = crate::pending_reply::load_pending("teams")
        .into_iter()
        .filter(|r| r.status == crate::pending_reply::PendingStatus::Pending)
        .collect::<Vec<_>>();

    if pending.is_empty() {
        return "目前沒有待確認的 Teams 草稿。".to_string();
    }

    pending
        .iter()
        .map(|r| format!(
            "ID: {}\n來自: {}\n原訊息: {}\n草稿: {}\n建立時間: {}",
            r.id, r.peer_name, r.original_message, r.draft_reply, r.created_at
        ))
        .collect::<Vec<_>>()
        .join("\n---\n")
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_teams_approve(args: Value) -> Result<String, String> {
    let id = args["id"].as_str().ok_or("Missing id")?;
    crate::pending_reply::update_status(
        "teams", id,
        crate::pending_reply::PendingStatus::Approved,
    );
    Ok(format!("草稿 {id} 已核准，等待送出。"))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_trigger_research(args: Value) -> Result<String, String> {
    let topic = args["topic"].as_str().ok_or("Missing topic")?.to_string();
    let url   = args["url"].as_str().map(|s| s.to_string());

    crate::events::publish(crate::events::AgentEvent::ResearchRequested {
        topic: topic.clone(),
        url,
    });
    Ok(format!("已觸發對「{topic}」的調研任務。"))
}

// ── Test runner MCP handlers ─────────────────────────────────────────────────

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_list_tests(args: Value) -> Result<Value, String> {
    let tag_filter = args.get("tag").and_then(Value::as_str);
    let tests = crate::test_runner::list_tests();
    let items: Vec<Value> = tests.iter()
        .filter(|t| match tag_filter {
            Some(tag) => t.tags.iter().any(|x| x == tag),
            None => true,
        })
        .map(|t| json!({
            "id":   t.id,
            "name": t.name,
            "url":  t.url,
            "goal": t.goal,
            "tags": t.tags,
            "max_iterations": t.max_iterations,
            "timeout_secs": t.timeout_secs,
            // Surface docs_refs so callers see required reading before running.
            "docs_refs": t.docs_refs,
        }))
        .collect();
    Ok(json!({ "count": items.len(), "tests": items }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_run_test_async(args: Value) -> Result<Value, String> {
    let test_id = args["test_id"].as_str().ok_or("Missing test_id")?.to_string();
    let auto_fix = args.get("auto_fix").and_then(Value::as_bool).unwrap_or(false);

    // Issue #269 — optional per-test LLM override. Parsed from the `llm_override`
    // arg as `{provider, model, base_url?, api_key?}`. Bad provider name returns
    // an error here (before spawn) so the caller learns immediately.
    let llm_override: Option<crate::llm::LlmOverride> = match args.get("llm_override") {
        Some(v) if !v.is_null() => Some(serde_json::from_value(v.clone())
            .map_err(|e| format!("invalid llm_override: {e}"))?),
        _ => None,
    };

    // Look up the test goal before spawning so we can surface docs_refs.
    // spawn_run_async will also look it up internally; this double-read is
    // cheap (small YAML dir) and lets us surface the warning before the run.
    let docs_refs = crate::test_runner::parser::find(&test_id)
        .map(|g| g.docs_refs)
        .unwrap_or_default();

    let run_id = crate::test_runner::spawn_run_async_with_override(
        test_id.clone(), auto_fix, llm_override.clone()
    )?;

    let mut resp = json!({
        "run_id": run_id,
        "test_id": test_id,
        "auto_fix": auto_fix,
        "status": "queued",
        "poll_with": "get_test_result",
    });
    if let Some(o) = llm_override.as_ref() {
        resp["llm_override"] = json!({
            "provider": o.provider,
            "model":    o.model,
        });
    }

    // Surface docs_refs as a hard-to-miss field so callers cannot skip
    // reading required documentation before interpreting results.
    if !docs_refs.is_empty() {
        resp["docs_refs"] = json!(&docs_refs);
        resp["warning"] = json!(format!(
            "⚠️ Read ALL {} doc(s) in docs_refs BEFORE running or interpreting this test.",
            docs_refs.len()
        ));
    }

    Ok(resp)
}

/// Spawn N tests in parallel, each on its own dedicated chrome tab.
///
/// `max_concurrency` clamped to [1, 8].  CDP isn't designed for hundreds
/// of simultaneous tabs — 8 is conservative; if you need a wider sweep,
/// shard externally and call this multiple times.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_run_test_batch(args: Value) -> Result<Value, String> {
    let test_ids: Vec<String> = args.get("test_ids")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .ok_or("Missing test_ids (array of strings)")?;
    if test_ids.is_empty() {
        return Err("test_ids is empty".into());
    }

    // ── Provider + replay-history aware concurrency cap (#267) ──────────────
    //
    // Two failure modes this guards against:
    //
    //   1. Gemini RPM stalls — running more parallel tests than the free-tier
    //      RPM can support causes every test to queue at the token-bucket gate.
    //      Total wall time goes UP, no test runs faster.
    //
    //   2. Local LLM (LM Studio) serialisation — single GPU model handles ONE
    //      decode at a time. concurrency=2 doubles per-iter latency, often
    //      pushing tests past their timeout (observed 2026-05-03: 3/5 tests
    //      timed out at concurrency=2 on Gemma 4 e4b).
    //
    // Predictive classification (#267 acceptance criteria):
    //   - replay_only test = last 3 runs ALL used saved scripts (no LLM expected
    //     this run either, barring script drift). Safe to schedule at full
    //     hardware cap, doesn't count against LLM safe_concurrency.
    //   - needs_llm test  = any of last 3 runs used LLM ReAct, OR no history.
    //     Counted against the LLM-provider safe_concurrency.
    //
    // Final cap = min(user requested, max(needs_llm safe count, replay_only count))
    //   — but we keep batch-wide concurrency simple: throttle to LLM safe if
    //     ANY needs_llm test is present, since they'll be the long pole anyway.
    //     Pure-replay batches still go full speed.
    let rpm = std::env::var("GEMINI_RPM")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|&r| r > 0.0)
        .unwrap_or(8.0);

    let provider = std::env::var("LLM_PROVIDER").unwrap_or_default().to_lowercase();
    let is_gemini = provider.contains("gemini");
    let is_local_lm = provider.contains("lmstudio") || provider.contains("ollama");

    // Classify each test (#267 acceptance: log per-test class).
    let needs_llm_count = test_ids
        .iter()
        .filter(|tid| {
            let needs_llm = crate::test_runner::store::test_needs_llm_predictively(tid, 3);
            tracing::info!(
                "[batch] scheduling {} ({})",
                tid,
                if needs_llm { "llm-fallback expected, throttled" } else { "replay-only" }
            );
            needs_llm
        })
        .count();
    let replay_only_count = test_ids.len() - needs_llm_count;

    // LLM safe concurrency derivation per provider.
    //   - Gemini cloud: existing RPM math
    //   - Local LM Studio / Ollama: hard cap at 1 (model serialises decode)
    //   - Other cloud: hardware cap (8)
    let llm_safe: usize = if is_gemini {
        let calls_per_min_per_test: f64 = test_ids
            .iter()
            .map(|tid| {
                let stats = crate::test_runner::store::test_stats(tid);
                if stats.avg_iterations > 0.0 && stats.avg_duration_ms > 0 {
                    stats.avg_iterations / (stats.avg_duration_ms as f64 / 60_000.0)
                } else {
                    10.0
                }
            })
            .sum::<f64>()
            / test_ids.len() as f64;
        ((rpm / calls_per_min_per_test).floor() as usize).max(1)
    } else if is_local_lm {
        1
    } else {
        8
    };

    // Batch-wide safe concurrency: if any needs_llm tests, throttle to llm_safe;
    // otherwise (pure replay batch) allow hardware cap.
    let safe_concurrency = if needs_llm_count > 0 {
        llm_safe
    } else {
        8
    };

    let raw_cap = args.get("max_concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(safe_concurrency as u64) as usize;

    let mut warn: Option<String> = None;
    let cap = if raw_cap > safe_concurrency && needs_llm_count > 0 {
        let provider_note = if is_gemini {
            format!("GEMINI_RPM={rpm:.0}")
        } else if is_local_lm {
            "local LLM serialises decode".to_string()
        } else {
            "non-Gemini cloud".to_string()
        };
        warn = Some(format!(
            "max_concurrency={raw_cap} exceeds LLM-safe limit={safe_concurrency} \
             ({needs_llm_count}/{} tests classified as llm-fallback expected; {provider_note}). \
             Clamping to {safe_concurrency} to prevent 429/timeout cascade.",
            test_ids.len()
        ));
        safe_concurrency
    } else {
        raw_cap.clamp(1, 8)
    };

    tracing::info!(
        "[batch] {} tests (needs_llm={}, replay_only={}), concurrency={}/{} (llm_safe={}, provider={})",
        test_ids.len(), needs_llm_count, replay_only_count, cap, raw_cap, llm_safe, provider
    );

    let run_ids = crate::test_runner::spawn_batch_run(test_ids.clone(), cap)?;
    // Pair each input test with its assigned run_id for client clarity.
    let pairs: Vec<Value> = test_ids.iter().zip(run_ids.iter())
        .map(|(tid, rid)| json!({ "test_id": tid, "run_id": rid }))
        .collect();
    let mut resp = json!({
        "count": pairs.len(),
        "max_concurrency": cap,
        "safe_concurrency": safe_concurrency,
        "rpm_limit": rpm,
        "runs": pairs,
        "status": "queued",
        "poll_each_with": "get_test_result",
    });
    if let Some(w) = warn {
        resp["warning"] = json!(w);
    }
    Ok(resp)
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_run_test_pipeline(args: Value) -> Result<Value, String> {
    let stage_ids: Vec<String> = args.get("stage_ids")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .ok_or("Missing stage_ids (array of strings)")?;
    if stage_ids.is_empty() {
        return Err("stage_ids is empty".into());
    }
    let stop_on_failure = args.get("stop_on_failure")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (pipeline_id, run_ids) =
        crate::test_runner::spawn_pipeline_run(stage_ids.clone(), stop_on_failure)?;

    let stages: Vec<Value> = stage_ids.iter().zip(run_ids.iter())
        .enumerate()
        .map(|(i, (tid, rid))| json!({
            "stage": i + 1,
            "test_id": tid,
            "run_id": rid,
        }))
        .collect();

    Ok(json!({
        "pipeline_id": pipeline_id,
        "stage_count": stages.len(),
        "stop_on_failure": stop_on_failure,
        "stages": stages,
        "status": "running",
        "poll_each_with": "get_test_result",
        "note": "Stages run sequentially — poll each run_id independently."
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_get_test_result(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?;
    let mut result = match crate::test_runner::runs::get(run_id) {
        Some(state) => crate::test_runner::runs::to_json(&state),
        None => return Err(format!("run_id '{run_id}' not found (may have been pruned)")),
    };

    // Issue #220: attach console log from SQLite (written at test completion).
    // Only available once the test has finished; returns null while running.
    let console_log = crate::test_runner::store::get_console_log(run_id);
    if let Some(ref log_json) = console_log {
        // Parse to compute a quick summary (error + warn counts) without
        // requiring callers to parse the raw JSON themselves.
        let (errors, warnings) = parse_console_counts(log_json);
        if let Some(obj) = result.as_object_mut() {
            obj.insert("console_log".into(), serde_json::Value::String(log_json.clone()));
            obj.insert("console_errors".into(), serde_json::Value::Number(errors.into()));
            obj.insert("console_warnings".into(), serde_json::Value::Number(warnings.into()));
        }
    } else {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("console_log".into(), serde_json::Value::Null);
            obj.insert("console_errors".into(), serde_json::Value::Number(0.into()));
            obj.insert("console_warnings".into(), serde_json::Value::Number(0.into()));
        }
    }

    Ok(result)
}

/// Portable home directory — USERPROFILE on Windows, HOME on Unix.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(std::path::PathBuf::from)
}

/// Parse a console_log JSON string (array of {level, text}) and return
/// (error_count, warning_count).  Gracefully returns (0, 0) on any parse error.
fn parse_console_counts(log_json: &str) -> (u32, u32) {
    let Ok(arr) = serde_json::from_str::<serde_json::Value>(log_json) else {
        return (0, 0);
    };
    let Some(msgs) = arr.as_array() else { return (0, 0) };
    let mut errors = 0u32;
    let mut warnings = 0u32;
    for msg in msgs {
        match msg.get("level").and_then(|l| l.as_str()) {
            Some("error")   => errors   += 1,
            Some("warning") => warnings += 1,
            _ => {}
        }
    }
    (errors, warnings)
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_kill_run(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?;
    match crate::test_runner::runs::kill_run(run_id) {
        Ok(()) => Ok(json!({
            "status": "killed",
            "run_id": run_id,
            "message": format!("run '{}' set to error state (killed by user)", run_id)
        })),
        Err(e) => Err(e),
    }
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_get_screenshot(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?;
    match crate::test_runner::runs::get_screenshot(run_id) {
        Some((Some(bytes), _)) => Ok(json!({
            "run_id": run_id,
            "mime": "image/png",
            "bytes_base64": base64_encode(&bytes),
            "size_bytes": bytes.len(),
        })),
        Some((None, Some(err))) => Ok(json!({
            "run_id": run_id,
            "bytes_base64": null,
            "screenshot_error": err,
        })),
        Some((None, None)) => Ok(json!({
            "run_id": run_id,
            "bytes_base64": null,
            "screenshot_error": "no screenshot captured (test passed or not yet taken)",
        })),
        None => Err(format!("run_id '{run_id}' not found")),
    }
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_run_adhoc_test(args: Value) -> Result<Value, String> {
    let url = args["url"].as_str().ok_or("Missing url")?.to_string();
    let goal = args["goal"].as_str().ok_or("Missing goal")?.to_string();
    let criteria: Vec<String> = args.get("success_criteria")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let locale = args.get("locale").and_then(Value::as_str).map(String::from);
    let max_iter = args.get("max_iterations").and_then(Value::as_u64).map(|n| n as u32);
    let timeout = args.get("timeout_secs").and_then(Value::as_u64);
    let headless = args.get("browser_headless").and_then(Value::as_bool);
    let llm_backend = args.get("llm_backend").and_then(Value::as_str).map(String::from);
    let fixture: Option<crate::test_runner::parser::Fixture> = args.get("fixture")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // Issue #81: optional per-run URL blocklist.
    let blocked_url_patterns: Option<Vec<String>> = args.get("blocked_url_patterns")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let run_id = crate::test_runner::spawn_adhoc_run(crate::test_runner::AdhocRunRequest {
        url: url.clone(),
        goal,
        success_criteria: criteria,
        locale,
        max_iterations: max_iter,
        timeout_secs: timeout,
        browser_headless: headless,
        llm_backend,
        fixture,
        blocked_url_patterns,
    })?;
    Ok(json!({
        "run_id": run_id,
        "url": url,
        "status": "queued",
        "poll_with": "get_test_result",
    }))
}

/// Promote a successful ad-hoc run into a permanent regression test
/// (writes `config/tests/<test_id>.yaml`).  See the schema description
/// for the full validation contract.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_persist_adhoc_run(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?.to_string();
    let test_id = args["test_id"].as_str().ok_or("Missing test_id")?.to_string();
    let name = args.get("name").and_then(Value::as_str).map(String::from);
    let tags: Option<Vec<String>> = args.get("tags")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect());
    // Default both flags to the SAFE option — refuse silent overwrites; bump
    // iterations so regression has slack vs. the (often tightly-fit) ad-hoc run.
    let bump_iterations = args.get("bump_iterations").and_then(Value::as_bool).unwrap_or(true);
    let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);

    let result = crate::test_runner::persist_adhoc_run(crate::test_runner::PersistAdhocParams {
        run_id,
        test_id,
        name,
        tags,
        bump_iterations,
        overwrite,
    })?;
    serde_json::to_value(&result).map_err(|e| format!("serialize result: {e}"))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_list_recent_runs(args: Value) -> Result<Value, String> {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20).min(100) as usize;
    let test_id = args.get("test_id").and_then(Value::as_str);

    let runs = match test_id {
        Some(tid) => crate::test_runner::store::recent_runs(tid, limit),
        None      => crate::test_runner::store::recent_runs_all(limit),
    };
    let items: Vec<Value> = runs.into_iter().map(|r| {
        let mut obj = json!({
            "id":               r.id,
            "test_id":          r.test_id,
            "started_at":       r.started_at,
            "duration_ms":      r.duration_ms,
            "status":           r.status,
            "failure_category": r.failure_category,
            "ai_analysis":      r.ai_analysis,
            "screenshot_path":  r.screenshot_path,
            "console_errors":   r.console_errors,
            "console_warnings": r.console_warnings,
        });
        // Surface a quick ⚠️ flag for tests with console errors so callers
        // don't need to parse counts themselves.
        if r.console_errors > 0 {
            obj["console_flag"] = serde_json::Value::String("error".into());
        } else if r.console_warnings > 0 {
            obj["console_flag"] = serde_json::Value::String("warning".into());
        }
        obj
    }).collect();
    Ok(json!({ "count": items.len(), "runs": items }))
}

/// `test_summary` — one-shot summary of the most recent batch / regression run.
/// Groups results by test_id (most recent win), computes console stats, and
/// flags tests with errors so callers see a clean pass/fail + console report.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_test_summary(args: Value) -> Result<Value, String> {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(31).min(100) as usize;

    // Default: runs from the last hour.
    let since_str = args.get("since").and_then(Value::as_str);
    let cutoff = since_str.map(String::from).unwrap_or_else(|| {
        let t = chrono::Local::now() - chrono::Duration::hours(1);
        t.format("%H:%M").to_string()
    });

    let all_runs = crate::test_runner::store::recent_runs_all(limit * 4);

    // Deduplicate by test_id, keep most recent per test.
    let mut seen: std::collections::HashMap<String, crate::test_runner::store::RunRecord> =
        std::collections::HashMap::new();
    for r in all_runs {
        if r.started_at.len() >= 16 && &r.started_at[11..16] >= cutoff.as_str() {
            seen.entry(r.test_id.clone()).or_insert(r);
        }
    }

    let mut results: Vec<Value> = seen.values().map(|r| {
        let flag = if r.console_errors > 0 { "error" }
                   else if r.console_warnings > 0 { "warning" }
                   else { "ok" };
        json!({
            "test_id":          r.test_id,
            "status":           r.status,
            "duration_ms":      r.duration_ms,
            "console_errors":   r.console_errors,
            "console_warnings": r.console_warnings,
            "flag":             flag,
        })
    }).collect();
    results.sort_by(|a, b| {
        // Sort: failures first, then console-error, then by test_id
        let a_bad = a["status"].as_str().map(|s| s != "passed").unwrap_or(false) as u8;
        let b_bad = b["status"].as_str().map(|s| s != "passed").unwrap_or(false) as u8;
        b_bad.cmp(&a_bad)
            .then(b["console_errors"].as_u64().cmp(&a["console_errors"].as_u64()))
            .then(a["test_id"].as_str().cmp(&b["test_id"].as_str()))
    });

    let total = results.len();
    let passed = results.iter().filter(|r| r["status"] == "passed").count();
    let failed = total - passed;
    let console_errors_total: u64 = results.iter()
        .filter_map(|r| r["console_errors"].as_u64()).sum();
    let console_warnings_total: u64 = results.iter()
        .filter_map(|r| r["console_warnings"].as_u64()).sum();

    let recommendation = if failed > 0 {
        format!("{} tests failed — check 'flag' and 'status' fields", failed)
    } else if console_errors_total > 0 {
        format!("All passed but {} console errors detected — review with get_test_result", console_errors_total)
    } else {
        format!("All {} tests passed with clean console ✓", total)
    };

    Ok(json!({
        "total":                  total,
        "passed":                 passed,
        "failed":                 failed,
        "console_errors_total":   console_errors_total,
        "console_warnings_total": console_warnings_total,
        "recommendation":         recommendation,
        "results":                results,
    }))
}

/// test_coverage — read agora_market.yaml feature map and compute coverage statistics.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_test_coverage(args: Value) -> Result<Value, String> {
    let group_filter = args.get("group_id").and_then(Value::as_str).map(String::from);
    let missing_only = args.get("show_missing_only").and_then(Value::as_bool).unwrap_or(false);

    // Load feature map config.
    let map_path = crate::platform::config_path("coverage/agora_market.yaml");
    let map_src = std::fs::read_to_string(&map_path)
        .map_err(|e| format!("Cannot read coverage map {map_path:?}: {e}"))?;
    let map: serde_json::Value = serde_yaml::from_str(&map_src)
        .map_err(|e| format!("Parse error in agora_market.yaml: {e}"))?;

    // Build a lookup: test_id → { has_script, pass_rate, last_status }
    let all_tests = crate::test_runner::list_tests();
    let all_scripts = crate::test_runner::store::all_test_stats();
    let all_saved: std::collections::HashMap<String, bool> = all_tests.iter()
        .map(|t| (t.id.clone(), crate::test_runner::store::script_info(&t.id).is_some()))
        .collect();
    let stats_map: std::collections::HashMap<&str, _> = all_scripts.iter()
        .map(|s| (s.test_id.as_str(), s))
        .collect();

    let groups_raw = map["feature_groups"].as_array()
        .ok_or("feature_groups missing or not array")?;

    let mut total_features = 0usize;
    let mut total_covered = 0usize;
    let mut all_gaps: Vec<Value> = Vec::new();
    let mut group_results: Vec<Value> = Vec::new();

    for g in groups_raw {
        let gid = g["id"].as_str().unwrap_or("?");
        if let Some(ref f) = group_filter {
            if gid != f { continue; }
        }
        let gname = g["name"].as_str().unwrap_or(gid);

        let features_raw = g["features"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        let mut covered_count = 0usize;
        let mut feature_results: Vec<Value> = Vec::new();

        for feat in features_raw {
            let fid = feat["id"].as_str().unwrap_or("?");
            let fname = feat["name"].as_str().unwrap_or(fid);
            let status = feat["status"].as_str().unwrap_or("missing");

            // Resolve which tests cover this feature.
            let test_ids: Vec<&str> = feat["test_ids"].as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            let covering_tests: Vec<Value> = test_ids.iter().map(|tid| {
                let has_script = all_saved.get(*tid).copied().unwrap_or(false);
                let stat = stats_map.get(tid);
                let pass_rate = stat.map(|s| s.pass_rate_7d).unwrap_or(0.0);
                let last_status = stat.and_then(|_| {
                    crate::test_runner::store::recent_runs(tid, 1).into_iter().next()
                        .map(|r| r.status)
                }).unwrap_or_else(|| "never_run".into());
                json!({
                    "test_id":      tid,
                    "has_script":   has_script,
                    "pass_rate_7d": pass_rate,
                    "last_status":  last_status,
                })
            }).collect();

            let is_covered = status != "missing" && !test_ids.is_empty();
            if is_covered { covered_count += 1; }

            if missing_only && status != "missing" { continue; }

            feature_results.push(json!({
                "id":       fid,
                "name":     fname,
                "status":   status,
                "tests":    covering_tests,
            }));

            if status == "missing" {
                all_gaps.push(json!({
                    "feature_id":    fid,
                    "feature_name":  fname,
                    "group_id":      gid,
                    "group_name":    gname,
                    "suggestion":    format!("新增 agora_{} 測試 YAML", fid),
                }));
            }
        }

        let feat_total = features_raw.len();
        let pct = if feat_total > 0 { (covered_count * 100 / feat_total) as u32 } else { 0 };
        total_features += feat_total;
        total_covered += covered_count;

        group_results.push(json!({
            "id":       gid,
            "name":     gname,
            "role":     g["role"],
            "covered":  covered_count,
            "total":    feat_total,
            "pct":      pct,
            "features": feature_results,
        }));
    }

    let overall_pct = if total_features > 0 {
        (total_covered * 100 / total_features) as u32
    } else { 0 };

    // Script summary across all regression tests.
    let total_tests = all_tests.len();
    let tests_with_script = all_saved.values().filter(|&&v| v).count();
    let tests_llm_only = all_tests.iter()
        .filter(|t| !all_saved.get(&t.id).copied().unwrap_or(false))
        .map(|t| t.id.as_str())
        .filter(|id| id.starts_with("agora_"))
        .collect::<Vec<_>>();

    Ok(json!({
        "product":    map["product"],
        "version":    map["version"],
        "overall": {
            "covered":  total_covered,
            "total":    total_features,
            "pct":      overall_pct,
        },
        "script_status": {
            "total_tests":        total_tests,
            "has_script":         tests_with_script,
            "llm_only_tests":     tests_llm_only,
        },
        "groups": group_results,
        "gaps":   all_gaps,
    }))
}

/// #247 — discover_app: kick off discovery crawler in a background thread.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_discover_app(args: Value) -> Result<Value, String> {
    let seed_url = args.get("seed_url").and_then(Value::as_str)
        .ok_or("seed_url required")?.to_string();
    let max_depth = args.get("max_depth").and_then(Value::as_u64)
        .map(|n| n as u32).unwrap_or(1);

    let run_id = format!(
        "disc_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S")
    );
    crate::test_runner::discovery::begin_run(&run_id, &seed_url, max_depth)?;

    let run_id_owned = run_id.clone();
    let seed_owned   = seed_url.clone();
    std::thread::spawn(move || {
        match crate::test_runner::discovery::crawl_app(
            &seed_owned, max_depth, &run_id_owned,
        ) {
            Ok(count) => {
                let _ = crate::test_runner::discovery::finish_run(
                    &run_id_owned, "done", Some(count), None,
                );
            }
            Err(e) => {
                let _ = crate::test_runner::discovery::finish_run(
                    &run_id_owned, "failed", Some(0), Some(&e),
                );
            }
        }
    });

    Ok(json!({
        "run_id":    run_id,
        "seed_url":  seed_url,
        "max_depth": max_depth,
        "note":      "crawl spawned; poll discovery_status for progress",
    }))
}

/// #247 — discovery_status: snapshot of latest run + counts.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_discovery_status() -> Result<Value, String> {
    let latest = crate::test_runner::discovery::latest_run()?;
    let total  = crate::test_runner::discovery::feature_count().unwrap_or(0);
    match latest {
        None => Ok(json!({
            "status":            "not_run",
            "total_features":    total,
            "note":              "discovery 從未執行；用 discover_app 啟動",
        })),
        Some(r) => Ok(json!({
            "status":            r.status,
            "run_id":            r.run_id,
            "started_at":        r.started_at,
            "finished_at":       r.finished_at,
            "total_widgets":     r.total_widgets,
            "error":             r.error,
            "seed_url":          r.seed_url,
            "max_depth":         r.max_depth,
            "total_features":    total,
        })),
    }
}

/// #247 — discovery_features: list discovered features (newest first).
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_discovery_features(args: Value) -> Result<Value, String> {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
    let kind_filter = args.get("kind").and_then(Value::as_str).map(String::from);

    let mut feats = crate::test_runner::discovery::list_features()?;
    if let Some(ref k) = kind_filter {
        feats.retain(|f| f.kind == *k);
    }
    let total = feats.len();
    feats.truncate(limit);

    Ok(json!({
        "total":     total,
        "returned":  feats.len(),
        "features":  feats.iter().map(|f| json!({
            "route":     f.route,
            "label":     f.label,
            "kind":      f.kind,
            "selector":  f.selector,
            "last_seen": f.last_seen,
            "run_id":    f.run_id,
        })).collect::<Vec<_>>(),
    }))
}

// (call_ui_navigate / call_ui_state removed in Phase 7 — UI is now a web
//  app at :7700/ui/, driven by browser-side fetch() not by an external bus.
//  AI testing of the UI uses /mcp + browser_exec on the same Chrome.)

/// #230 — list_flaky_tests: tests with pass_rate < threshold over recent runs.
/// Issue #239 — scaffold_test: generate a baseline regression YAML for the
/// given role + test_id.  Caller writes the returned YAML to disk; we do
/// NOT auto-write to avoid overwriting an existing test.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_scaffold_test(args: Value) -> Result<Value, String> {
    use crate::test_runner::scaffold::{scaffold_yaml, TestRole};

    let role_str = args.get("role").and_then(Value::as_str)
        .ok_or("missing required `role` (buyer / seller / delivery / admin)")?;
    let role = TestRole::from_str(role_str)
        .ok_or_else(|| format!(
            "invalid role `{role_str}` — expected buyer / seller / delivery / admin"
        ))?;
    let test_id = args.get("id").and_then(Value::as_str)
        .ok_or("missing required `id` (test_id, same as YAML file basename)")?;
    if test_id.is_empty() || test_id.contains('/') || test_id.contains('\\') {
        return Err(format!("invalid test_id `{test_id}` — must be a plain identifier"));
    }
    let name = args.get("name").and_then(Value::as_str);

    let yaml = scaffold_yaml(role, test_id, name);
    // Suggested write location.  We compose the path with `platform::config_path`
    // so the suggestion always matches where the runner would actually look.
    let suggested_path = crate::platform::config_path(
        std::path::Path::new("tests")
            .join("agora_regression")
            .join(format!("{test_id}.yaml"))
    );

    Ok(json!({
        "role":            role_str,
        "id":              test_id,
        "yaml":            yaml,
        "suggested_path":  suggested_path.to_string_lossy(),
        "next_steps": [
            "Save the YAML to suggested_path (or wherever you keep tests)",
            "Replace TODO placeholders in the goal block with real action steps",
            "Replace TODO placeholders in success_criteria with positive-presence assertions",
            "Run `lint_test` on the test_id to verify it passes all 5 lint rules",
        ],
    }))
}

/// Issue #239 — lint_test: run all 5 YAML lint rules on a single test by id.
/// Returns issues as structured JSON; empty array = clean.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_lint_test(args: Value) -> Result<Value, String> {
    let test_id = args.get("test_id").and_then(Value::as_str)
        .ok_or("missing required `test_id`")?;
    let goal = crate::test_runner::parser::find(test_id)
        .ok_or_else(|| format!("test `{test_id}` not found in config/tests/"))?;
    let issues = crate::test_runner::lint::lint(&goal);
    let issues_json: Vec<Value> = issues.iter().map(|i| json!({
        "rule":     i.rule,
        "severity": match i.severity {
            crate::test_runner::lint::Severity::Warn  => "warn",
            crate::test_runner::lint::Severity::Error => "error",
        },
        "step":     i.step,
        "message":  i.message,
    })).collect();
    Ok(json!({
        "test_id":     test_id,
        "clean":       issues.is_empty(),
        "issue_count": issues.len(),
        "issues":      issues_json,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_list_flaky_tests(args: Value) -> Result<Value, String> {
    let threshold = args.get("threshold").and_then(Value::as_f64).unwrap_or(0.70);
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;

    let all_stats = crate::test_runner::store::all_test_stats();
    let mut flaky: Vec<Value> = all_stats.iter()
        .filter(|s| s.total_runs >= 3 && s.pass_rate_7d < threshold)
        .map(|s| json!({
            "test_id":              s.test_id,
            "pass_rate_7d":         s.pass_rate_7d,
            "pass_rate_30d":        s.pass_rate_30d,
            "total_runs":           s.total_runs,
            "avg_iterations":       s.avg_iterations,
            "avg_duration_ms":      s.avg_duration_ms,
            "top_failure_category": s.top_failure_category,
            "has_script":           crate::test_runner::store::script_info(&s.test_id).is_some(),
        }))
        .take(limit)
        .collect();
    // Worst first
    flaky.sort_by(|a, b| a["pass_rate_7d"].as_f64().partial_cmp(&b["pass_rate_7d"].as_f64())
        .unwrap_or(std::cmp::Ordering::Equal));

    Ok(json!({
        "count":     flaky.len(),
        "threshold": threshold,
        "tests":     flaky,
    }))
}

/// #230 — explain_failure: LLM-generated root-cause explanation for a test run.
/// Combines console_log, history, ai_analysis, and failure_category into a
/// human-readable diagnostic report.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_explain_failure(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?;

    // Try in-memory state first, fall back to SQLite (handles pruned runs).
    let mem_state = crate::test_runner::runs::get(run_id);

    let (status, ai_analysis, error_msg, history_summary, console_log) = if let Some(ref s) = mem_state {
        let json = crate::test_runner::runs::to_json(s);
        let status = json["status"].as_str().unwrap_or("unknown").to_string();
        let details = &json["details"];
        let ai = details["analysis"].as_str().map(String::from);
        let err = details["error"].as_str().map(String::from);
        let hist = match &s.phase {
            crate::test_runner::runs::RunPhase::Complete(r) => {
                r.history.iter().rev().take(5).rev().enumerate()
                    .map(|(i, step)| format!("  {}: {} → {}", i+1,
                        &step.action.to_string()[..step.action.to_string().len().min(60)],
                        &step.observation[..step.observation.len().min(120)]))
                    .collect::<Vec<_>>().join("\n")
            }
            _ => String::new(),
        };
        let console = crate::test_runner::store::get_console_log(run_id);
        (status, ai, err, hist, console)
    } else {
        // In-memory pruned — pull from SQLite.
        match crate::test_runner::store::find_full_context_by_run_id(run_id) {
            Some((status, cat, ai, steps, console)) => {
                let hist: String = steps.unwrap_or_default();
                let err: Option<String> = cat.map(|c| format!("failure_category: {c}"));
                (status, ai, err, hist, console)
            }
            None => return Err(format!("run_id '{run_id}' not found (not in memory or SQLite)")),
        }
    };

    if status == "passed" {
        return Ok(json!({
            "run_id": run_id,
            "status": "passed",
            "explanation": "Test passed — no failure to explain.",
        }));
    }

    // Parse console errors.
    let console_section: String = console_log.as_deref().map(|log| {
        let msgs: Vec<String> = serde_json::from_str::<Value>(log).ok()
            .and_then(|v| v.as_array().cloned()).unwrap_or_default()
            .into_iter()
            .filter(|m| m.get("level").and_then(|l| l.as_str()) == Some("error"))
            .take(5)
            .filter_map(|m| m["text"].as_str().map(|t| format!("  [error] {}", &t[..t.len().min(200)])))
            .collect();
        if msgs.is_empty() { String::new() }
        else { format!("\nBrowser Console Errors:\n{}", msgs.join("\n")) }
    }).unwrap_or_default();

    // Build the prompt for LLM explanation.
    let prompt = format!(
        "You are a test failure analyst. Explain the ROOT CAUSE of this browser E2E test failure in 3-5 sentences. \
         Be specific about WHAT failed and WHY. Output only the explanation, no JSON.\n\n\
         Test ID: {run_id}\n\
         Status: {status}\n\
         Error: {err}\n\
         Existing AI analysis: {ai}\n\
         Last steps:\n{hist}{console}\n\n\
         Root cause explanation:",
        run_id = run_id,
        status = status,
        err = error_msg.as_deref().unwrap_or("(none)"),
        ai = ai_analysis.as_deref().unwrap_or("(none)"),
        hist = if history_summary.is_empty() { "  (not available)".to_string() } else { history_summary.clone() },
        console = console_section,
    );

    // Use the main LLM (fallback to simple summary if unavailable).
    let ctx = crate::adk::context::AgentContext::new("explain_failure",
        crate::adk::tool::ToolRegistry::new());
    let explanation = match crate::llm::call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => format!("LLM unavailable: {e}. Raw info: status={status}, error={:?}", error_msg),
    };

    Ok(json!({
        "run_id":           run_id,
        "status":           status,
        "explanation":      explanation,
        "console_errors":   console_log.as_deref().map(parse_console_counts).map(|(e,_)| e).unwrap_or(0),
        "ai_analysis":      ai_analysis,
    }))
}

/// #266 — script_health: project-wide replay metrics + per-test classification.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_script_health(args: Value) -> Result<Value, String> {
    let window_days = args.get("window_days")
        .and_then(Value::as_u64)
        .map(|v| v as u32)
        .unwrap_or(14);

    let aggregate = crate::test_runner::store::replay_health(window_days);

    // Per-test breakdown — same shape the UI uses, exposed via MCP for cron /
    // CI consumers that want to flag specific stale tests.
    let per_test: Vec<Value> = crate::test_runner::list_tests()
        .into_iter()
        .map(|t| {
            let status = crate::test_runner::store::test_replay_status(&t.id);
            let recent = crate::test_runner::store::recent_replay_modes(&t.id, 3);
            let has_script = crate::test_runner::store::script_info(&t.id).is_some();
            let status_str = match status {
                crate::test_runner::store::ReplayStatus::Healthy        => "healthy",
                crate::test_runner::store::ReplayStatus::StaleFallback  => "stale_fallback",
                crate::test_runner::store::ReplayStatus::LlmOnly        => "llm_only",
                crate::test_runner::store::ReplayStatus::Untested       => "untested",
            };
            json!({
                "test_id":     t.id,
                "status":      status_str,
                "has_script":  has_script,
                "recent_modes": recent,
            })
        })
        .collect();

    Ok(json!({
        "aggregate": aggregate,
        "per_test":  per_test,
        "drift_warning_threshold": -0.10,
    }))
}

/// #230 — replayLastFailure: step-by-step inspection of the last failed run.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_replay_last_failure(args: Value) -> Result<Value, String> {
    let test_id  = args["test_id"].as_str().ok_or("Missing test_id")?;
    let break_at = args.get("break_at").and_then(Value::as_u64).unwrap_or(0) as usize;

    let row = crate::test_runner::store::last_failed_run(test_id)
        .ok_or_else(|| format!("No failed run found for test_id={test_id}"))?;

    let (run_id, started_at, duration_ms, failure_category, ai_analysis,
         iterations, history_json, console_log) = row;

    // Parse steps from history_json.
    let steps_raw: Vec<Value> = history_json
        .as_deref()
        .and_then(|h| serde_json::from_str::<Value>(h).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let steps_total = steps_raw.len();
    let steps_shown: Vec<Value> = steps_raw
        .into_iter()
        .enumerate()
        .take(if break_at == 0 { usize::MAX } else { break_at })
        .map(|(i, s)| {
            let action  = s.get("action").and_then(Value::as_str).unwrap_or("?").to_string();
            let args_v  = s.get("args").cloned().unwrap_or(Value::Null);
            let result  = s.get("result").and_then(Value::as_str).unwrap_or("").to_string();
            let obs     = s.get("observation").and_then(Value::as_str).unwrap_or("").to_string();
            json!({
                "step":        i + 1,
                "action":      action,
                "args":        args_v,
                "result":      result,
                "observation": obs,
            })
        })
        .collect();

    let console_errors = console_log.as_deref().map(parse_console_counts).map(|(e,_)| e).unwrap_or(0);

    Ok(json!({
        "test_id":          test_id,
        "run_id":           run_id,
        "started_at":       started_at,
        "duration_ms":      duration_ms,
        "failure_category": failure_category,
        "iterations":       iterations,
        "console_errors":   console_errors,
        "ai_analysis":      ai_analysis,
        "steps_total":      steps_total,
        "steps_shown":      steps_shown.len(),
        "steps":            steps_shown,
    }))
}

/// #230 — shadowDumpDiff: unified diff of LLM observations at step A vs step B.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_shadow_dump_diff(args: Value) -> Result<Value, String> {
    let test_id = args["test_id"].as_str().ok_or("Missing test_id")?;
    let step_a  = args["step_a"].as_u64().ok_or("Missing step_a")? as usize;
    let step_b  = args["step_b"].as_u64().ok_or("Missing step_b")? as usize;
    let run_id_override = args.get("run_id").and_then(Value::as_str);

    // Get history_json from the specified run or latest failed run.
    let history_json: Option<String> = if let Some(rid) = run_id_override {
        crate::test_runner::store::find_history_by_run_id(rid)
            .and_then(|(_, _, _, hj)| hj)
    } else {
        crate::test_runner::store::last_failed_run(test_id)
            .and_then(|(_, _, _, _, _, _, hj, _)| hj)
    };

    let steps_raw: Vec<Value> = history_json
        .as_deref()
        .and_then(|h| serde_json::from_str::<Value>(h).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let steps_total = steps_raw.len();

    // Helper to extract observation at a 1-based step index.
    let get_obs = |n: usize| -> Result<String, String> {
        if n == 0 || n > steps_total {
            return Err(format!(
                "step {n} out of range (run has {steps_total} steps)"
            ));
        }
        Ok(steps_raw[n - 1]
            .get("observation")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    };

    let obs_a = get_obs(step_a)?;
    let obs_b = get_obs(step_b)?;

    // Simple line-level unified diff (no external crate needed).
    let lines_a: Vec<&str> = obs_a.lines().collect();
    let lines_b: Vec<&str> = obs_b.lines().collect();

    let removed: Vec<String> = lines_a.iter()
        .filter(|l| !lines_b.contains(l))
        .map(|l| format!("- {l}"))
        .collect();
    let added: Vec<String> = lines_b.iter()
        .filter(|l| !lines_a.contains(l))
        .map(|l| format!("+ {l}"))
        .collect();
    let unchanged_count = lines_a.iter().filter(|l| lines_b.contains(l)).count();

    let diff = if removed.is_empty() && added.is_empty() {
        "  (no differences in observation text)".to_string()
    } else {
        format!("{}\n{}", removed.join("\n"), added.join("\n"))
    };

    let action_a = steps_raw[step_a - 1].get("action").and_then(Value::as_str).unwrap_or("?");
    let action_b = steps_raw[step_b - 1].get("action").and_then(Value::as_str).unwrap_or("?");

    Ok(json!({
        "test_id":         test_id,
        "steps_total":     steps_total,
        "step_a": { "n": step_a, "action": action_a, "lines": lines_a.len() },
        "step_b": { "n": step_b, "action": action_b, "lines": lines_b.len() },
        "unchanged_lines": unchanged_count,
        "removed_lines":   removed.len(),
        "added_lines":     added.len(),
        "diff":            diff,
        "obs_a":           obs_a,
        "obs_b":           obs_b,
    }))
}

/// #230 compareWithReplay — diff most recent script run vs most recent LLM run.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_compare_with_replay(args: Value) -> Result<Value, String> {
    let test_id = args["test_id"].as_str().ok_or("Missing test_id")?;

    // Fetch recent runs for this test_id (up to 20, enough to find both kinds).
    let runs = crate::test_runner::store::recent_runs(test_id, 20);
    if runs.is_empty() {
        return Err(format!("No runs found for test_id={test_id}"));
    }

    // Split into replay (is_replay=true) and LLM (is_replay=false) runs.
    let latest_replay = runs.iter().find(|r| r.is_replay);
    let latest_llm    = runs.iter().find(|r| !r.is_replay);

    let to_summary = |r: &crate::test_runner::store::RunRecord| json!({
        "mode":             if r.is_replay { "script" } else { "llm" },
        "status":          r.status,
        "started_at":      r.started_at,
        "duration_ms":     r.duration_ms,
        "failure_category": r.failure_category,
        "console_errors":  r.console_errors,
        "console_warnings": r.console_warnings,
    });

    let replay_summary = latest_replay.map(to_summary);
    let llm_summary    = latest_llm.map(to_summary);

    // Compute comparison if both exist.
    let comparison: Option<Value> = match (latest_replay, latest_llm) {
        (Some(r), Some(l)) => {
            let both_passed  = r.status == "passed" && l.status == "passed";
            let both_failed  = r.status != "passed" && l.status != "passed";
            let same_outcome = r.status == l.status;
            let duration_delta_ms: i64 = match (r.duration_ms, l.duration_ms) {
                (Some(rd), Some(ld)) => rd - ld,
                _ => 0,
            };
            let verdict = if both_passed {
                "✅ 兩者都 PASS — replay 可靠"
            } else if both_failed {
                "⚠️ 兩者都 FAIL — 可能是真實 bug，不是 replay 問題"
            } else if r.status == "passed" && l.status != "passed" {
                "⚡ replay PASS 但 LLM FAIL — LLM 行為不一致，replay 更穩定"
            } else {
                "🔴 LLM PASS 但 replay FAIL — script 可能過期，需重新錄製"
            };
            Some(json!({
                "same_outcome":     same_outcome,
                "verdict":          verdict,
                "duration_delta_ms": duration_delta_ms,
                "replay_faster":    duration_delta_ms < 0,
            }))
        }
        _ => None,
    };

    // Has a script at all?
    let has_script = crate::test_runner::store::script_info(test_id).is_some();

    Ok(json!({
        "test_id":        test_id,
        "has_script":     has_script,
        "replay_run":     replay_summary,
        "llm_run":        llm_summary,
        "comparison":     comparison,
        "note": if !has_script {
            "no saved script — run a test first to record a script"
        } else if latest_replay.is_none() {
            "no replay run found yet — trigger a replay to compare"
        } else { "" },
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_test_analytics(args: Value) -> Result<Value, String> {
    let test_id = args.get("test_id").and_then(Value::as_str);
    let stats = match test_id {
        Some(tid) => vec![crate::test_runner::store::test_stats(tid)],
        None      => crate::test_runner::store::all_test_stats(),
    };
    // Named regression tests only (adhoc_* excluded, min 3 runs enforced in store).
    let total_tests = stats.len();
    let flaky_count = stats.iter().filter(|s| s.is_flaky).count();
    let avg_pass_rate = if total_tests == 0 { 0.0 } else {
        stats.iter().map(|s| s.pass_rate_7d).sum::<f64>() / total_tests as f64
    };
    let items: Vec<Value> = stats.iter().map(|s| json!({
        "test_id":              s.test_id,
        "total_runs":           s.total_runs,
        "pass_rate_7d":         s.pass_rate_7d,
        "pass_rate_30d":        s.pass_rate_30d,
        "is_flaky":             s.is_flaky,
        "avg_iterations":       s.avg_iterations,
        "avg_duration_ms":      s.avg_duration_ms,
        "top_failure_category": s.top_failure_category,
        // Script replay info (#136)
        "has_script":           crate::test_runner::store::script_info(&s.test_id).is_some(),
    })).collect();
    Ok(json!({
        "tests":   items,
        "summary": {
            "total_tests":   total_tests,   // named regression only (adhoc_* excluded, ≥3 runs)
            "flaky_count":   flaky_count,
            "avg_pass_rate": avg_pass_rate,
        }
    }))
}

// ── #229 Permission Allowlist ─────────────────────────────────────────────────

/// Scan ~/.claude/projects/**/*.jsonl and suggest allowlist patterns.
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_suggest_allowlist(args: Value) -> Result<Value, String> {
    use std::collections::HashMap;

    let threshold   = args.get("threshold").and_then(Value::as_u64).unwrap_or(2) as usize;
    let sessions    = args.get("sessions").and_then(Value::as_u64).unwrap_or(10) as usize;
    let proj_filter = args.get("project_key").and_then(Value::as_str).map(String::from);

    // Locate ~/.claude/projects
    let home = home_dir().ok_or("Cannot determine home directory")?;
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.exists() {
        return Err(format!("Projects dir not found: {projects_dir:?}"));
    }

    // Collect all JSONL files across project dirs.
    let mut jsonl_files: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() { continue; }
            if let Some(ref key) = proj_filter {
                let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !dir_name.contains(key.as_str()) { continue; }
            }
            // Flat JSONL files directly inside each project dir.
            if let Ok(files) = std::fs::read_dir(&dir) {
                for f in files.flatten() {
                    let p = f.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        jsonl_files.push(p);
                    }
                }
            }
        }
    }

    // Sort by modification time, newest first; take last N sessions.
    jsonl_files.sort_by(|a, b| {
        let mt_a = a.metadata().and_then(|m| m.modified()).ok();
        let mt_b = b.metadata().and_then(|m| m.modified()).ok();
        mt_b.cmp(&mt_a)
    });
    jsonl_files.truncate(sessions);

    // Read current allowlist from settings.json.
    let settings_path = home.join(".claude").join("settings.json");
    let existing_allow: std::collections::HashSet<String> = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["permissions"]["allow"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    // Count tool-call patterns across scanned files.
    let mut counter: HashMap<String, usize> = HashMap::new();
    let mut files_scanned = 0usize;

    for fpath in &jsonl_files {
        let content = match std::fs::read_to_string(fpath) {
            Ok(c) => c,
            Err(_) => continue,
        };
        files_scanned += 1;
        for line in content.lines() {
            let Ok(obj) = serde_json::from_str::<Value>(line) else { continue };
            let msg = obj.get("message").unwrap_or(&obj);
            let content_arr = msg.get("content").and_then(Value::as_array);
            let Some(arr) = content_arr else { continue };
            for item in arr {
                let Some(typ) = item.get("type").and_then(Value::as_str) else { continue };
                if typ != "tool_use" { continue; }
                let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
                let pattern = if name == "Bash" {
                    let cmd = item.get("input")
                        .and_then(|i| i.get("command"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let first = cmd.split_whitespace().next().unwrap_or("?");
                    // Strip path prefixes, keep executable name only.
                    let exe = first.rsplit(&['/', '\\']).next().unwrap_or(first);
                    // Remove characters that aren't alphanumeric, underscore, dot, or dash.
                    let exe: String = exe.chars()
                        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '.' || *c == '-')
                        .collect();
                    if exe.is_empty() { "Bash(?:*)".to_string() }
                    else { format!("Bash({exe}:*)") }
                } else {
                    name.to_string()
                };
                *counter.entry(pattern).or_insert(0) += 1;
            }
        }
    }

    // Build suggestions: high-frequency AND not already in allowlist.
    let mut suggestions: Vec<Value> = counter.iter()
        .filter(|(pat, &cnt)| cnt >= threshold && !existing_allow.contains(*pat))
        .map(|(pat, &cnt)| json!({ "pattern": pat, "frequency": cnt }))
        .collect();
    suggestions.sort_by(|a, b| {
        b["frequency"].as_u64().cmp(&a["frequency"].as_u64())
    });

    Ok(json!({
        "files_scanned":  files_scanned,
        "sessions_limit": sessions,
        "threshold":      threshold,
        "new_suggestions": suggestions.len(),
        "suggestions":    suggestions,
    }))
}

/// List redundant entries in ~/.claude/settings.json allowlist.
/// A pattern is redundant if it is strictly covered by another wildcard pattern
/// already in the list (e.g. `Bash(git log:*)` is redundant when `Bash(git:*)` exists).
pub(crate) fn call_list_redundant_allow() -> Result<Value, String> {
    let home = home_dir().ok_or("Cannot determine home directory")?;
    let settings_path = home.join(".claude").join("settings.json");
    let src = std::fs::read_to_string(&settings_path)
        .map_err(|e| format!("Cannot read settings.json: {e}"))?;
    let parsed: Value = serde_json::from_str(&src)
        .map_err(|e| format!("Parse error: {e}"))?;
    let allow: Vec<String> = parsed["permissions"]["allow"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    // Wildcards: patterns that end with :*)
    let wildcards: Vec<&str> = allow.iter()
        .filter(|p| p.ends_with(":*)"))
        .map(String::as_str)
        .collect();

    // For each pattern, check if it's strictly covered by a wildcard.
    let mut redundant: Vec<Value> = Vec::new();
    for pat in &allow {
        if !pat.ends_with(":*)") {
            // Specific pattern — check if any wildcard covers it.
            for wc in &wildcards {
                // A pattern `Bash(git log:*)` is covered by `Bash(git:*)`
                // if pat starts with wc_prefix where wc_prefix = wc without the trailing ":*)".
                let wc_prefix = &wc[..wc.len() - 3]; // drop ":*)"
                if pat.starts_with(wc_prefix) && pat != *wc {
                    redundant.push(json!({
                        "pattern":     pat,
                        "covered_by":  wc,
                        "suggestion":  "remove",
                    }));
                    break;
                }
            }
            continue;
        }
        // Wildcard pattern — check if another wildcard is a strict prefix.
        for other_wc in &wildcards {
            if *other_wc == pat.as_str() { continue; }
            let other_prefix = &other_wc[..other_wc.len() - 3];
            let self_prefix  = &pat[..pat.len() - 3];
            if self_prefix.starts_with(other_prefix) && self_prefix != other_prefix {
                redundant.push(json!({
                    "pattern":    pat,
                    "covered_by": other_wc,
                    "suggestion": "remove",
                }));
                break;
            }
        }
    }

    Ok(json!({
        "total_allow_entries": allow.len(),
        "redundant_count":     redundant.len(),
        "redundant":           redundant,
    }))
}

// ── #225 Claude Config Management ────────────────────────────────────────────

/// Read ~/.claude/settings.json, apply a mutating closure, write back atomically.
fn mutate_settings<F>(f: F) -> Result<(), String>
where
    F: FnOnce(&mut Value) -> Result<(), String>,
{
    let home = home_dir().ok_or("Cannot determine home directory")?;
    let path = home.join(".claude").join("settings.json");
    let src  = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read settings.json: {e}"))?;
    let mut parsed: Value = serde_json::from_str(&src)
        .map_err(|e| format!("Parse error: {e}"))?;
    f(&mut parsed)?;
    let out = serde_json::to_string_pretty(&parsed)
        .map_err(|e| format!("Serialize error: {e}"))?;
    std::fs::write(&path, out).map_err(|e| format!("Write error: {e}"))?;
    Ok(())
}

pub(crate) fn call_list_allowlist() -> Result<Value, String> {
    let home = home_dir().ok_or("Cannot determine home directory")?;
    let path = home.join(".claude").join("settings.json");
    let src  = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read settings.json: {e}"))?;
    let parsed: Value = serde_json::from_str(&src)
        .map_err(|e| format!("Parse error: {e}"))?;
    let allow: Vec<String> = parsed["permissions"]["allow"]
        .as_array().unwrap_or(&vec![])
        .iter().filter_map(|v| v.as_str().map(String::from)).collect();

    let wildcard_count = allow.iter().filter(|p| p.ends_with(":*)")).count();
    let exact_count    = allow.len() - wildcard_count;

    Ok(json!({
        "total":    allow.len(),
        "wildcard": wildcard_count,
        "exact":    exact_count,
        "entries":  allow,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_add_allow(args: Value) -> Result<Value, String> {
    let pattern = args["pattern"].as_str().ok_or("Missing pattern")?.to_string();
    let mut already_existed = false;
    mutate_settings(|v| {
        let arr = v["permissions"]["allow"]
            .as_array_mut()
            .ok_or_else(|| "permissions.allow is not an array".to_string())?;
        let pat_val = Value::String(pattern.clone());
        if arr.contains(&pat_val) {
            already_existed = true;
        } else {
            arr.push(pat_val);
        }
        Ok(())
    })?;
    Ok(json!({
        "pattern":        pattern,
        "added":          !already_existed,
        "already_existed": already_existed,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_remove_allow(args: Value) -> Result<Value, String> {
    let pattern = args["pattern"].as_str().ok_or("Missing pattern")?.to_string();
    let mut removed = false;
    mutate_settings(|v| {
        let arr = v["permissions"]["allow"]
            .as_array_mut()
            .ok_or_else(|| "permissions.allow is not an array".to_string())?;
        let before = arr.len();
        arr.retain(|v| v.as_str() != Some(pattern.as_str()));
        removed = arr.len() < before;
        Ok(())
    })?;
    Ok(json!({ "pattern": pattern, "removed": removed }))
}

pub(crate) fn call_list_slash_commands() -> Result<Value, String> {
    let home = home_dir().ok_or("Cannot determine home directory")?;
    let cmd_dir = home.join(".claude").join("commands");
    if !cmd_dir.exists() {
        return Ok(json!({ "commands": [], "note": "~/.claude/commands/ not found" }));
    }
    let mut cmds: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&cmd_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") { continue; }
            let name = p.file_stem().and_then(|s| s.to_str())
                .unwrap_or("?").to_string();
            let body = std::fs::read_to_string(&p).unwrap_or_default();
            // Extract first non-empty line as description.
            let desc = body.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim_start_matches('#')
                .trim()
                .to_string();
            cmds.push(json!({ "name": name, "description": desc }));
        }
    }
    cmds.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({ "count": cmds.len(), "commands": cmds }))
}

pub(crate) fn call_list_hooks() -> Result<Value, String> {
    let home = home_dir().ok_or("Cannot determine home directory")?;
    let path = home.join(".claude").join("settings.json");
    let src  = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read settings.json: {e}"))?;
    let parsed: Value = serde_json::from_str(&src)
        .map_err(|e| format!("Parse error: {e}"))?;
    let hooks = parsed.get("hooks").cloned().unwrap_or(Value::Object(Default::default()));
    Ok(json!({ "hooks": hooks }))
}

// ── #227 Session Memory (save points) ────────────────────────────────────────

fn session_points_path() -> Option<std::path::PathBuf> {
    home_dir().map(|h| h.join(".claude").join("session_points.json"))
}

fn load_points() -> Vec<Value> {
    session_points_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn save_points(points: &[Value]) -> Result<(), String> {
    let path = session_points_path().ok_or("Cannot determine home directory")?;
    let out = serde_json::to_string_pretty(&Value::Array(points.to_vec()))
        .map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| e.to_string())
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_save_point(args: Value) -> Result<Value, String> {
    let label    = args["label"].as_str().ok_or("Missing label")?.to_string();
    let summary  = args.get("summary").and_then(Value::as_str).unwrap_or("").to_string();
    let ttl_days = args.get("ttl_days").and_then(Value::as_f64).unwrap_or(7.0) as u64;

    let now = chrono::Local::now();
    let saved_at  = now.to_rfc3339();
    let expire_at = (now + chrono::Duration::days(ttl_days as i64)).to_rfc3339();

    let mut points = load_points();
    // Upsert by label.
    let existing_idx = points.iter().position(|p| p["label"].as_str() == Some(&label));
    let entry = json!({
        "label":     label,
        "summary":   summary,
        "saved_at":  saved_at,
        "expire_at": expire_at,
        "ttl_days":  ttl_days,
    });
    if let Some(i) = existing_idx {
        points[i] = entry.clone();
    } else {
        points.push(entry.clone());
    }
    save_points(&points)?;
    Ok(json!({ "saved": entry, "total_points": points.len() }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_list_points(args: Value) -> Result<Value, String> {
    let filter = args.get("label_contains").and_then(Value::as_str).map(String::from);
    let now = chrono::Local::now().to_rfc3339();

    let points = load_points();
    let active: Vec<&Value> = points.iter()
        .filter(|p| p["expire_at"].as_str().map(|e| e > now.as_str()).unwrap_or(true))
        .filter(|p| {
            if let Some(ref f) = filter {
                p["label"].as_str().map(|l| l.contains(f.as_str())).unwrap_or(false)
            } else {
                true
            }
        })
        .collect();

    Ok(json!({
        "count":  active.len(),
        "points": active,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_restore_point(args: Value) -> Result<Value, String> {
    let label = args["label"].as_str().ok_or("Missing label")?;
    let points = load_points();
    let point = points.iter()
        .find(|p| p["label"].as_str() == Some(label))
        .ok_or_else(|| format!("Save point '{label}' not found"))?;
    Ok(point.clone())
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_expire_points() -> Result<Value, String> {
    let now = chrono::Local::now().to_rfc3339();
    let all = load_points();
    let before = all.len();
    let active: Vec<Value> = all.into_iter()
        .filter(|p| p["expire_at"].as_str().map(|e| e > now.as_str()).unwrap_or(true))
        .collect();
    let removed = before - active.len();
    save_points(&active)?;
    Ok(json!({ "removed": removed, "remaining": active.len() }))
}

// ── #232 Session Cost Tracking ────────────────────────────────────────────────

/// Anthropic model pricing (USD per million tokens, as of 2025-05).
/// Format: (input_mtok, output_mtok, cache_write_mtok, cache_read_mtok)
fn model_pricing(model: &str) -> (f64, f64, f64, f64) {
    match model {
        m if m.contains("opus")   => (15.0, 75.0, 18.75, 1.50),
        m if m.contains("sonnet") => (3.0,  15.0,  3.75,  0.30),
        m if m.contains("haiku")  => (0.25,  1.25,  0.30,  0.03),
        _                         => (3.0,  15.0,  3.75,  0.30), // default sonnet
    }
}

struct SessionTokens {
    session_id: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total_messages: u64,
    cost_usd: f64,
}

fn parse_session(path: &std::path::Path) -> Option<SessionTokens> {
    let content = std::fs::read_to_string(path).ok()?;
    let session_id = path.file_stem()?.to_str()?.to_string();
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut total_messages = 0u64;
    let mut cost_usd = 0.0f64;

    for line in content.lines() {
        let Ok(obj) = serde_json::from_str::<Value>(line) else { continue };
        let msg = obj.get("message").unwrap_or(&obj);
        let Some(usage) = msg.get("usage") else { continue };
        let model = msg.get("model").and_then(Value::as_str).unwrap_or("sonnet");
        let (pi, po, pw, pr) = model_pricing(model);

        let inp  = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let out  = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cr   = usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cw   = usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);

        input += inp; output += out; cache_read += cr; cache_write += cw;
        total_messages += 1;

        cost_usd += (inp as f64 / 1_000_000.0) * pi
            + (out as f64 / 1_000_000.0) * po
            + (cr  as f64 / 1_000_000.0) * pr
            + (cw  as f64 / 1_000_000.0) * pw;
    }

    if total_messages == 0 { return None; }
    Some(SessionTokens { session_id, input, output, cache_read, cache_write, total_messages, cost_usd })
}

fn collect_jsonl_files(project_key: Option<&str>) -> Vec<std::path::PathBuf> {
    let home = match home_dir() { Some(h) => h, None => return Vec::new() };
    let projects_dir = home.join(".claude").join("projects");
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for e in entries.flatten() {
            let dir = e.path();
            if !dir.is_dir() { continue; }
            if let Some(key) = project_key {
                let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.contains(key) { continue; }
            }
            if let Ok(fents) = std::fs::read_dir(&dir) {
                for f in fents.flatten() {
                    let p = f.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                        files.push(p);
                    }
                }
            }
        }
    }
    files
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_session_cost(args: Value) -> Result<Value, String> {
    let session_id = args.get("session_id").and_then(Value::as_str).map(String::from);
    let proj_key   = args.get("project_key").and_then(Value::as_str).map(String::from);

    let files = collect_jsonl_files(proj_key.as_deref());
    if files.is_empty() {
        return Err("No JSONL files found in ~/.claude/projects".to_string());
    }

    let target_file: Option<std::path::PathBuf> = if let Some(ref sid) = session_id {
        files.into_iter().find(|p| {
            p.file_stem().and_then(|s| s.to_str()) == Some(sid.as_str())
        })
    } else {
        // Latest file by modification time.
        files.into_iter().max_by_key(|p| {
            p.metadata().and_then(|m| m.modified()).ok()
        })
    };

    let path = target_file.ok_or_else(|| format!("Session '{}' not found", session_id.as_deref().unwrap_or("latest")))?;
    let t = parse_session(&path).ok_or("No usage data found in session")?;

    let cache_hit_pct = if t.cache_read + t.cache_write > 0 {
        (t.cache_read as f64 / (t.cache_read + t.cache_write) as f64) * 100.0
    } else { 0.0 };

    Ok(json!({
        "session_id":     t.session_id,
        "file":           path.to_string_lossy(),
        "tokens": {
            "input":       t.input,
            "output":      t.output,
            "cache_read":  t.cache_read,
            "cache_write": t.cache_write,
            "total":       t.input + t.output + t.cache_read + t.cache_write,
        },
        "cache_hit_pct":  cache_hit_pct,
        "messages":       t.total_messages,
        "cost_usd":       format!("{:.4}", t.cost_usd),
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_list_expensive_sessions(args: Value) -> Result<Value, String> {
    let top      = args.get("top").and_then(Value::as_u64).unwrap_or(10) as usize;
    let proj_key = args.get("project_key").and_then(Value::as_str).map(String::from);

    let files = collect_jsonl_files(proj_key.as_deref());
    let mut sessions: Vec<Value> = files.iter()
        .filter_map(|p| parse_session(p))
        .map(|t| json!({
            "session_id":  t.session_id,
            "cost_usd":    t.cost_usd,
            "cost_usd_fmt": format!("{:.4}", t.cost_usd),
            "input":       t.input,
            "output":      t.output,
            "cache_read":  t.cache_read,
            "messages":    t.total_messages,
        }))
        .collect();

    sessions.sort_by(|a, b| {
        b["cost_usd"].as_f64().partial_cmp(&a["cost_usd"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sessions.truncate(top);

    let total_usd: f64 = sessions.iter()
        .filter_map(|s| s["cost_usd"].as_f64())
        .sum();

    Ok(json!({
        "top":         top,
        "shown":       sessions.len(),
        "total_usd":   format!("{:.4}", total_usd),
        "sessions":    sessions,
    }))
}

// ── #228 Task Tracker ─────────────────────────────────────────────────────────

fn tasks_path() -> Option<std::path::PathBuf> {
    home_dir().map(|h| h.join(".claude").join("tasks.json"))
}

fn load_tasks() -> Vec<Value> {
    tasks_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn persist_tasks(tasks: &[Value]) -> Result<(), String> {
    let path = tasks_path().ok_or("Cannot determine home directory")?;
    let out = serde_json::to_string_pretty(&Value::Array(tasks.to_vec()))
        .map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| e.to_string())
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_create_task(args: Value) -> Result<Value, String> {
    let project  = args["project"].as_str().ok_or("Missing project")?.to_string();
    let desc     = args["description"].as_str().ok_or("Missing description")?.to_string();
    let priority = args.get("priority").and_then(Value::as_str).unwrap_or("P1").to_string();
    let kb_refs  = args.get("kb_refs").and_then(Value::as_str).unwrap_or("").to_string();

    let now = chrono::Local::now();
    let date_str = now.format("%Y%m%d").to_string();
    let mut tasks = load_tasks();
    let seq = tasks.iter()
        .filter(|t| t["id"].as_str().map(|s| s.starts_with(&format!("T-{date_str}"))).unwrap_or(false))
        .count() + 1;
    let id = format!("T-{date_str}-{seq:04}");

    let task = json!({
        "id":          id.clone(),
        "project":     project,
        "description": desc,
        "priority":    priority,
        "status":      "open",
        "kb_refs":     kb_refs,
        "created_at":  now.to_rfc3339(),
        "done_at":     null,
        "resolution":  null,
        "github_url":  null,
    });
    tasks.push(task.clone());
    persist_tasks(&tasks)?;
    Ok(json!({ "task_id": id, "task": task }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_list_tasks(args: Value) -> Result<Value, String> {
    let project_filter  = args.get("project").and_then(Value::as_str).map(String::from);
    let status_filter   = args.get("status").and_then(Value::as_str).unwrap_or("open");
    let priority_filter = args.get("priority").and_then(Value::as_str).map(String::from);

    let tasks = load_tasks();
    let filtered: Vec<&Value> = tasks.iter()
        .filter(|t| {
            let status = t["status"].as_str().unwrap_or("open");
            match status_filter {
                "all"  => true,
                "open" => status == "open",
                "done" => status == "done",
                other  => status == other,
            }
        })
        .filter(|t| {
            if let Some(ref proj) = project_filter {
                t["project"].as_str() == Some(proj.as_str())
            } else { true }
        })
        .filter(|t| {
            if let Some(ref pri) = priority_filter {
                t["priority"].as_str() == Some(pri.as_str())
            } else { true }
        })
        .collect();

    Ok(json!({
        "total":  filtered.len(),
        "filter": { "project": project_filter, "status": status_filter, "priority": priority_filter },
        "tasks":  filtered,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_mark_task_done(args: Value) -> Result<Value, String> {
    let task_id    = args["task_id"].as_str().ok_or("Missing task_id")?;
    let resolution = args["resolution"].as_str().ok_or("Missing resolution")?.to_string();
    let now = chrono::Local::now().to_rfc3339();

    let mut tasks = load_tasks();
    let idx = tasks.iter().position(|t| t["id"].as_str() == Some(task_id))
        .ok_or_else(|| format!("Task '{task_id}' not found"))?;
    if let Some(obj) = tasks[idx].as_object_mut() {
        obj.insert("status".to_string(), Value::String("done".to_string()));
        obj.insert("done_at".to_string(), Value::String(now));
        obj.insert("resolution".to_string(), Value::String(resolution));
    }
    let updated = tasks[idx].clone();
    persist_tasks(&tasks)?;
    Ok(updated)
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_link_task(args: Value) -> Result<Value, String> {
    let task_id     = args["task_id"].as_str().ok_or("Missing task_id")?;
    let github_url  = args.get("github_url").and_then(Value::as_str).map(String::from);
    let kb_topickey = args.get("kb_topickey").and_then(Value::as_str).map(String::from);

    let mut tasks = load_tasks();
    let idx = tasks.iter().position(|t| t["id"].as_str() == Some(task_id))
        .ok_or_else(|| format!("Task '{task_id}' not found"))?;
    if let Some(obj) = tasks[idx].as_object_mut() {
        if let Some(ref url) = github_url {
            obj.insert("github_url".to_string(), Value::String(url.clone()));
        }
        if let Some(ref key) = kb_topickey {
            obj.insert("kb_topickey".to_string(), Value::String(key.clone()));
        }
    }
    let updated = tasks[idx].clone();
    persist_tasks(&tasks)?;
    Ok(updated)
}

// ── #224 Handoff MCP ─────────────────────────────────────────────────────────

fn handoff_history_path() -> Option<std::path::PathBuf> {
    home_dir().map(|h| h.join(".claude").join("handoff_history.json"))
}

fn load_handoffs() -> Vec<Value> {
    handoff_history_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn save_handoffs(entries: &[Value]) -> Result<(), String> {
    let path = handoff_history_path().ok_or("Cannot determine home directory")?;
    let out = serde_json::to_string_pretty(&Value::Array(entries.to_vec()))
        .map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| e.to_string())
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_create_handoff(args: Value) -> Result<Value, String> {
    let reason    = args["reason"].as_str().ok_or("Missing reason")?.to_string();
    let content   = args["content"].as_str().ok_or("Missing content")?.to_string();
    let project   = args.get("project").and_then(Value::as_str).unwrap_or("sirin").to_string();
    let file_refs = args.get("file_refs").and_then(Value::as_str).unwrap_or("").to_string();

    let now = chrono::Local::now();
    let saved_at = now.to_rfc3339();
    let id = format!("{}-handoff", now.format("%Y%m%d-%H%M%S"));

    let entry = json!({
        "id":        id.clone(),
        "reason":    reason.clone(),
        "project":   project.clone(),
        "file_refs": file_refs,
        "saved_at":  saved_at.clone(),
        "content":   content.clone(),
    });

    // Prepend (newest first) and keep last 50.
    let mut history = load_handoffs();
    history.insert(0, entry.clone());
    history.truncate(50);
    save_handoffs(&history)?;

    // Best-effort write to KB (agora-trading) for SessionStart hook compatibility.
    // Errors here are non-fatal — local file is the primary store.
    let kb_status = try_kb_write_handoff(&content, &reason, &project, &file_refs);

    Ok(json!({
        "id":         id,
        "saved_at":   saved_at,
        "project":    project,
        "history_len": history.len(),
        "kb_write":   kb_status,
        "tip":        "Retrieve with get_latest_handoff. SessionStart hook reads from KB.",
    }))
}

/// Non-blocking best-effort call to agora-trading kbWrite for cross-session
/// compatibility with the existing fetch-handoff.sh mechanism.
fn try_kb_write_handoff(content: &str, reason: &str, project: &str, file_refs: &str) -> String {
    // Read tokens from ~/.claude.json
    let read_token = |server: &str| -> Option<String> {
        let path = home_dir()?.join(".claude.json");
        let src = std::fs::read_to_string(&path).ok()?;
        let v: Value = serde_json::from_str(&src).ok()?;
        v["mcpServers"][server]["headers"]["Authorization"]
            .as_str()
            .map(String::from)
    };

    let trading_tok = match read_token("agora-trading") {
        Some(t) => t,
        None => return "skipped (no agora-trading token)".to_string(),
    };
    let ops_tok = match read_token("agora-ops") {
        Some(t) => t,
        None => return "skipped (no agora-ops token)".to_string(),
    };

    // 2026-05-05 — project-specific topicKey to avoid cross-project collision.
    // See KB `sirin-process-mcp-handoff-workflow` v3 for the full rationale:
    // before this, every project (sirin / agora-backend / flutter) wrote to
    // the single `sirin-handoff-latest` key and clobbered each other.  Now
    // each project gets its own `<project>-handoff-latest`.
    let topic_key = format!("{project}-handoff-latest");

    let payload = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "kbWrite",
            "arguments": {
                "topicKey":   topic_key,
                "title":      format!("Mid-session Handoff — {reason}"),
                "content":    content,
                "domain":     "ops",
                "layer":      "raw",
                "tags":       "handoff,session-bridge",
                "status":     "confirmed",
                "confidence": 0.95,
                "fileRefs":   file_refs,
                "source":     "claude-session",
                "project":    project,
            }
        },
        "id": 1
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_default();

    match client.post("https://agoramarketapi.purrtechllc.com/api/mcp")
        .header("Authorization", &trading_tok)
        .header("X-OPS-Authorization", &ops_tok)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&payload)
        .send()
    {
        Ok(resp) if resp.status().is_success() => "ok".to_string(),
        Ok(resp) => format!("http {}", resp.status()),
        Err(e)   => format!("err: {e}"),
    }
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_get_latest_handoff(args: Value) -> Result<Value, String> {
    let project = args.get("project").and_then(Value::as_str).unwrap_or("sirin");

    let history = load_handoffs();
    let latest = history.iter()
        .find(|e| e["project"].as_str().unwrap_or("sirin") == project)
        .ok_or_else(|| format!("No handoff found for project={project}"))?;

    Ok(json!({
        "id":       latest["id"],
        "reason":   latest["reason"],
        "saved_at": latest["saved_at"],
        "project":  latest["project"],
        "content":  latest["content"],   // raw markdown, already unescaped
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_list_handoff_history(args: Value) -> Result<Value, String> {
    let project = args.get("project").and_then(Value::as_str).unwrap_or("sirin");
    let limit   = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;

    let history = load_handoffs();
    let entries: Vec<Value> = history.iter()
        .filter(|e| e["project"].as_str().unwrap_or("sirin") == project)
        .take(limit)
        .map(|e| {
            // Return summary (first 120 chars of content) instead of full content.
            let preview: String = e["content"].as_str().unwrap_or("")
                .lines().next().unwrap_or("")
                .chars().take(120).collect();
            json!({
                "id":       e["id"],
                "reason":   e["reason"],
                "saved_at": e["saved_at"],
                "preview":  preview,
            })
        })
        .collect();

    Ok(json!({
        "project": project,
        "count":   entries.len(),
        "history": entries,
    }))
}

// ── #226 KB Lifecycle ────────────────────────────────────────────────────────

/// Compute Jaccard similarity between two texts using word bags.
fn jaccard_similarity(a: &str, b: &str) -> f64 {
    use std::collections::HashSet;
    let words_a: HashSet<&str> = a.split_whitespace().collect();
    let words_b: HashSet<&str> = b.split_whitespace().collect();
    let intersection = words_a.intersection(&words_b).count();
    let union        = words_a.union(&words_b).count();
    if union == 0 { return 0.0; }
    intersection as f64 / union as f64
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_kb_duplicate_check(args: Value) -> Result<Value, String> {
    let keys_str  = args["topic_keys"].as_str().ok_or("Missing topic_keys")?;
    let project   = args.get("project").and_then(Value::as_str).unwrap_or("sirin");
    let threshold = args.get("threshold").and_then(Value::as_f64).unwrap_or(0.7);

    let keys: Vec<&str> = keys_str.split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
    if keys.len() < 2 {
        return Err("Need at least 2 topic_keys to compare".to_string());
    }

    // Fetch all entries concurrently.
    let mut fetch_handles = Vec::new();
    for key in &keys {
        let k = key.to_string();
        let p = project.to_string();
        fetch_handles.push(tokio::spawn(async move {
            kb_get_via_http(&k, &p).await.map(|c| (k, c))
        }));
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    let mut errors:  Vec<String>            = Vec::new();
    for h in fetch_handles {
        match h.await {
            Ok(Ok((k, c))) => entries.push((k, c)),
            Ok(Err(e))     => errors.push(e),
            Err(e)         => errors.push(e.to_string()),
        }
    }

    // All-pairs Jaccard comparison.
    let mut duplicates: Vec<Value> = Vec::new();
    for i in 0..entries.len() {
        for j in (i + 1)..entries.len() {
            let score = jaccard_similarity(&entries[i].1, &entries[j].1);
            if score >= threshold {
                duplicates.push(json!({
                    "pair_a":  entries[i].0,
                    "pair_b":  entries[j].0,
                    "jaccard": (score * 1000.0).round() / 1000.0,
                    "suggestion": if score >= 0.9 {
                        "高度重複，建議 kb_merge"
                    } else if score >= 0.7 {
                        "部分重疊，建議 kb_diff 確認後考慮 kb_merge"
                    } else {
                        "低重疊"
                    }
                }));
            }
        }
    }

    // Sort most-similar first.
    duplicates.sort_by(|a, b| {
        b["jaccard"].as_f64().partial_cmp(&a["jaccard"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(json!({
        "project":         project,
        "keys_checked":    entries.len(),
        "threshold":       threshold,
        "duplicate_pairs": duplicates.len(),
        "duplicates":      duplicates,
        "fetch_errors":    errors,
    }))
}

/// Call agora-trading kbGet via HTTP and return the content string.
async fn kb_get_via_http(topic_key: &str, project: &str) -> Result<String, String> {
    let (trading_tok, ops_tok) = {
        let read_tok = |server: &str| -> Option<String> {
            let path = home_dir()?.join(".claude.json");
            let src = std::fs::read_to_string(path).ok()?;
            let v: Value = serde_json::from_str(&src).ok()?;
            v["mcpServers"][server]["headers"]["Authorization"]
                .as_str().map(String::from)
        };
        (
            read_tok("agora-trading").ok_or("Missing agora-trading token")?,
            read_tok("agora-ops").ok_or("Missing agora-ops token")?,
        )
    };

    let payload = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "kbGet",
            "arguments": { "topicKey": topic_key, "project": project }
        },
        "id": 1
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .post("https://agoramarketapi.purrtechllc.com/api/mcp")
        .header("Authorization", &trading_tok)
        .header("X-OPS-Authorization", &ops_tok)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;

    // Parse SSE / JSON response, extract text content.
    for line in resp.lines() {
        let line = line.trim().trim_start_matches("data: ");
        if !line.starts_with('{') { continue; }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["result"]["isError"].as_bool() == Some(true) {
            return Err(format!("kbGet error: {}", v["result"]["content"][0]["text"]));
        }
        if let Some(text) = v["result"]["content"][0]["text"].as_str() {
            return Ok(text.to_string());
        }
    }
    Err(format!("kbGet: no content returned for {topic_key}@{project}"))
}

/// Same pattern but for kbHealth.
async fn kb_health_via_http(project: &str) -> Result<String, String> {
    let read_tok = |server: &str| -> Option<String> {
        let path = home_dir()?.join(".claude.json");
        let src = std::fs::read_to_string(path).ok()?;
        let v: Value = serde_json::from_str(&src).ok()?;
        v["mcpServers"][server]["headers"]["Authorization"]
            .as_str().map(String::from)
    };
    let trading_tok = read_tok("agora-trading").ok_or("Missing agora-trading token")?;
    let ops_tok     = read_tok("agora-ops").ok_or("Missing agora-ops token")?;

    let payload = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": { "name": "kbHealth", "arguments": { "project": project } },
        "id": 1
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post("https://agoramarketapi.purrtechllc.com/api/mcp")
        .header("Authorization", &trading_tok)
        .header("X-OPS-Authorization", &ops_tok)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&payload)
        .send().await.map_err(|e| e.to_string())?
        .text().await.map_err(|e| e.to_string())?;

    for line in resp.lines() {
        let line = line.trim().trim_start_matches("data: ");
        if !line.starts_with('{') { continue; }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(text) = v["result"]["content"][0]["text"].as_str() {
            return Ok(text.to_string());
        }
    }
    Err("kbHealth: no content returned".to_string())
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_kb_stats(args: Value) -> Result<Value, String> {
    let project = args.get("project").and_then(Value::as_str).unwrap_or("sirin").to_string();

    // Get health text and parse the structured sections.
    let health_raw = kb_health_via_http(&project).await
        .unwrap_or_else(|e| format!("kbHealth error: {e}"));

    // Parse counts from the health text (format: "  key   N").
    let mut by_status: std::collections::HashMap<String, u64> = Default::default();
    let mut by_layer:  std::collections::HashMap<String, u64> = Default::default();
    let mut by_domain: std::collections::HashMap<String, u64> = Default::default();
    let mut total = 0u64;
    let mut section = "";
    for line in health_raw.lines() {
        let t = line.trim();
        if t.starts_with("total:") {
            total = t.split_whitespace().last().and_then(|s| s.parse().ok()).unwrap_or(0);
        } else if t == "by status:" { section = "status"; }
        else if t == "by layer:"  { section = "layer"; }
        else if t == "by domain:" { section = "domain"; }
        else if !t.is_empty() && !section.is_empty() {
            let parts: Vec<&str> = t.split_whitespace().collect();
            if parts.len() >= 2 {
                let key = parts[0].to_string();
                let val: u64 = parts[1].parse().unwrap_or(0);
                match section {
                    "status" => { by_status.insert(key, val); }
                    "layer"  => { by_layer.insert(key, val); }
                    "domain" => { by_domain.insert(key, val); }
                    _ => {}
                }
            }
        }
    }

    let confirmed  = *by_status.get("confirmed").unwrap_or(&0);
    let stale      = *by_status.get("stale").unwrap_or(&0);
    let draft      = *by_status.get("draft").unwrap_or(&0);
    let stale_pct  = if total > 0 { stale * 100 / total } else { 0 };
    let draft_pct  = if total > 0 { draft * 100 / total } else { 0 };

    Ok(json!({
        "project":    project,
        "total":      total,
        "by_status":  by_status,
        "by_layer":   by_layer,
        "by_domain":  by_domain,
        "ratios": {
            "stale_pct":     stale_pct,
            "draft_pct":     draft_pct,
            "confirmed_pct": if total > 0 { confirmed * 100 / total } else { 0 },
        },
        "health_flag": if stale_pct > 10 { "⚠️ stale > 10%" } else { "✅ healthy" },
        "raw_health":  health_raw,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_kb_diff(args: Value) -> Result<Value, String> {
    let topic_a   = args["topic_a"].as_str().ok_or("Missing topic_a")?;
    let topic_b   = args["topic_b"].as_str().ok_or("Missing topic_b")?;
    let project_a = args.get("project_a").and_then(Value::as_str).unwrap_or("sirin");
    let project_b = args.get("project_b").and_then(Value::as_str).unwrap_or(project_a);

    // Fetch both entries concurrently.
    let (res_a, res_b) = tokio::join!(
        kb_get_via_http(topic_a, project_a),
        kb_get_via_http(topic_b, project_b),
    );
    let content_a = res_a?;
    let content_b = res_b?;

    let lines_a: Vec<&str> = content_a.lines().collect();
    let lines_b: Vec<&str> = content_b.lines().collect();

    let removed: Vec<String> = lines_a.iter()
        .filter(|l| !lines_b.contains(l))
        .map(|l| format!("- {l}"))
        .collect();
    let added: Vec<String> = lines_b.iter()
        .filter(|l| !lines_a.contains(l))
        .map(|l| format!("+ {l}"))
        .collect();
    let unchanged = lines_a.iter().filter(|l| lines_b.contains(l)).count();

    let diff = if removed.is_empty() && added.is_empty() {
        "  (identical content)".to_string()
    } else {
        format!("{}\n{}", removed.join("\n"), added.join("\n"))
    };

    // Rough overlap ratio.
    let overlap_pct = if lines_a.len() + lines_b.len() > 0 {
        unchanged * 200 / (lines_a.len() + lines_b.len())
    } else { 0 };

    Ok(json!({
        "topic_a":      topic_a,
        "topic_b":      topic_b,
        "project_a":    project_a,
        "project_b":    project_b,
        "lines_a":      lines_a.len(),
        "lines_b":      lines_b.len(),
        "unchanged":    unchanged,
        "removed":      removed.len(),
        "added":        added.len(),
        "overlap_pct":  overlap_pct,
        "diff":         diff,
    }))
}

// ── #233 Cross-AI Router ─────────────────────────────────────────────────────

fn intents_path() -> Option<std::path::PathBuf> {
    home_dir().map(|h| h.join(".claude").join("llm_intents.json"))
}

fn load_intents() -> std::collections::HashMap<String, Value> {
    intents_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

fn save_intents(intents: &std::collections::HashMap<String, Value>) -> Result<(), String> {
    let path = intents_path().ok_or("Cannot determine home directory")?;
    let obj  = serde_json::Value::Object(intents.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
    let out  = serde_json::to_string_pretty(&obj).map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| e.to_string())
}

/// Build an LlmConfig for the given backend name.
/// For "deepseek" reads LLM_FALLBACK_* env vars; others use defaults.
fn llm_config_for_backend(backend: &str, model_override: Option<&str>, key_override: Option<&str>) -> crate::llm::LlmConfig {
    let lower = backend.to_lowercase();
    let backend_norm = match lower.as_str() {
        "deepseek" => "lmstudio", // OpenAI-compat
        other => other,
    };
    // For DeepSeek, read fallback env vars.
    let (model, api_key, base_url_override) = if lower == "deepseek" {
        let m = model_override
            .map(String::from)
            .or_else(|| std::env::var("LLM_FALLBACK_MODEL").ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| "deepseek-chat".to_string());
        let k = key_override
            .map(String::from)
            .or_else(|| std::env::var("LLM_FALLBACK_API_KEY").ok().filter(|v| !v.is_empty()));
        let url = std::env::var("LLM_FALLBACK_BASE_URL").ok()
            .filter(|v| !v.is_empty());
        (m, k, url)
    } else {
        let m = model_override.map(String::from)
            .unwrap_or_else(|| "gemini-2.0-flash".to_string());
        let k = key_override.map(String::from)
            .or_else(|| std::env::var("GEMINI_API_KEY").ok().filter(|v| !v.is_empty()));
        (m, k, None)
    };

    let mut cfg = crate::llm::LlmConfig::for_override(backend_norm, &model, api_key);
    if let Some(url) = base_url_override {
        cfg.base_url = url;
    }
    cfg
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_route_query(args: Value) -> Result<Value, String> {
    let intent = args["intent"].as_str().ok_or("Missing intent")?;
    let prompt  = args["prompt"].as_str().ok_or("Missing prompt")?.to_string();

    let intents = load_intents();
    let (backend, model) = if let Some(entry) = intents.get(intent) {
        let b = entry["backend"].as_str().unwrap_or("gemini").to_string();
        let m = entry["model"].as_str().map(String::from);
        (b, m)
    } else {
        ("gemini".to_string(), None)
    };

    let cfg = llm_config_for_backend(&backend, model.as_deref(), None);
    let ctx = crate::adk::context::AgentContext::new("route_query", crate::adk::tool::ToolRegistry::new());
    let start = std::time::Instant::now();
    let result = crate::llm::call_prompt(ctx.http.as_ref(), &cfg, prompt.clone())
        .await
        .map_err(|e| e.to_string())?;

    Ok(json!({
        "intent":      intent,
        "backend":     backend,
        "model":       cfg.model,
        "elapsed_ms":  start.elapsed().as_millis(),
        "result":      result,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_query_llm(args: Value) -> Result<Value, String> {
    let backend    = args["backend"].as_str().ok_or("Missing backend")?;
    let prompt     = args["prompt"].as_str().ok_or("Missing prompt")?.to_string();
    let model_ov   = args.get("model").and_then(Value::as_str);
    let key_ov     = args.get("api_key").and_then(Value::as_str);

    let cfg = llm_config_for_backend(backend, model_ov, key_ov);
    let ctx = crate::adk::context::AgentContext::new("query_llm", crate::adk::tool::ToolRegistry::new());
    let start = std::time::Instant::now();
    let result = crate::llm::call_prompt(ctx.http.as_ref(), &cfg, prompt)
        .await
        .map_err(|e| e.to_string())?;

    Ok(json!({
        "backend":    backend,
        "model":      cfg.model,
        "elapsed_ms": start.elapsed().as_millis(),
        "result":     result,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_fallback_chain(args: Value) -> Result<Value, String> {
    let prompt   = args["prompt"].as_str().ok_or("Missing prompt")?.to_string();
    let backends_str = args["backends"].as_str().ok_or("Missing backends")?;
    let backends: Vec<&str> = backends_str.split(',').map(str::trim).collect();

    let ctx = crate::adk::context::AgentContext::new("fallback_chain", crate::adk::tool::ToolRegistry::new());
    let start = std::time::Instant::now();

    for backend in &backends {
        let cfg = llm_config_for_backend(backend, None, None);
        match crate::llm::call_prompt(ctx.http.as_ref(), &cfg, prompt.clone()).await {
            Ok(result) => {
                return Ok(json!({
                    "backend_used": backend,
                    "elapsed_ms":   start.elapsed().as_millis(),
                    "attempts":     backends.iter().position(|b| b == backend).unwrap_or(0) + 1,
                    "result":       result,
                }));
            }
            Err(e) => {
                eprintln!("fallback_chain: {backend} failed: {e}");
                // Continue to next backend.
            }
        }
    }
    Err(format!("All backends failed: {backends_str}"))
}

pub(crate) fn call_list_intents() -> Result<Value, String> {
    let intents = load_intents();
    let list: Vec<Value> = intents.iter()
        .map(|(name, entry)| json!({
            "intent":  name,
            "backend": entry["backend"],
            "model":   entry.get("model"),
            "reason":  entry.get("reason"),
        }))
        .collect();
    Ok(json!({ "count": list.len(), "intents": list }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_register_intent(args: Value) -> Result<Value, String> {
    let name    = args["name"].as_str().ok_or("Missing name")?.to_string();
    let backend = args["backend"].as_str().ok_or("Missing backend")?.to_string();
    let model   = args.get("model").and_then(Value::as_str).map(String::from);
    let reason  = args.get("reason").and_then(Value::as_str).unwrap_or("").to_string();

    let mut intents = load_intents();
    let entry = json!({
        "backend": backend,
        "model":   model,
        "reason":  reason,
    });
    intents.insert(name.clone(), entry.clone());
    save_intents(&intents)?;
    Ok(json!({ "registered": name, "entry": entry, "total": intents.len() }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_benchmark_llms(args: Value) -> Result<Value, String> {
    let prompt   = args["prompt"].as_str().ok_or("Missing prompt")?.to_string();
    let backends_str = args.get("backends").and_then(Value::as_str).unwrap_or("gemini,deepseek");
    let backends: Vec<String> = backends_str.split(',').map(|s| s.trim().to_string()).collect();

    let ctx = std::sync::Arc::new(
        crate::adk::context::AgentContext::new("benchmark_llms", crate::adk::tool::ToolRegistry::new())
    );

    let mut handles = Vec::new();
    for backend in &backends {
        let b    = backend.clone();
        let p    = prompt.clone();
        let ctx2 = ctx.clone();
        handles.push(tokio::spawn(async move {
            let cfg   = llm_config_for_backend(&b, None, None);
            let start = std::time::Instant::now();
            let res   = crate::llm::call_prompt(ctx2.http.as_ref(), &cfg, p).await;
            let elapsed = start.elapsed().as_millis();
            (b, cfg.model, elapsed, res)
        }));
    }

    let mut results: Vec<Value> = Vec::new();
    for handle in handles {
        if let Ok((backend, model, elapsed_ms, res)) = handle.await {
            results.push(json!({
                "backend":    backend,
                "model":      model,
                "elapsed_ms": elapsed_ms,
                "ok":         res.is_ok(),
                "preview":    res.ok().map(|s| s.chars().take(200).collect::<String>()),
            }));
        }
    }

    // Sort fastest first.
    results.sort_by_key(|r| r["elapsed_ms"].as_u64().unwrap_or(u64::MAX));

    Ok(json!({
        "prompt_preview": prompt.chars().take(80).collect::<String>(),
        "backends_tested": results.len(),
        "results":         results,
    }))
}

// ── #231 Daily Brief ─────────────────────────────────────────────────────────

/// Call any agora-trading tool via HTTP and return raw text response.
async fn call_agora_tool(name: &str, arguments: Value) -> Result<String, String> {
    let read_tok = |server: &str| -> Option<String> {
        let path = home_dir()?.join(".claude.json");
        let src = std::fs::read_to_string(path).ok()?;
        let v: Value = serde_json::from_str(&src).ok()?;
        v["mcpServers"][server]["headers"]["Authorization"].as_str().map(String::from)
    };
    let trading_tok = read_tok("agora-trading").ok_or("Missing agora-trading token")?;
    let ops_tok     = read_tok("agora-ops").ok_or("Missing agora-ops token")?;

    let payload = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
        "id": 1
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build().map_err(|e| e.to_string())?;
    let resp = client.post("https://agoramarketapi.purrtechllc.com/api/mcp")
        .header("Authorization", &trading_tok)
        .header("X-OPS-Authorization", &ops_tok)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&payload)
        .send().await.map_err(|e| e.to_string())?
        .text().await.map_err(|e| e.to_string())?;

    for line in resp.lines() {
        let line = line.trim().trim_start_matches("data: ");
        if !line.starts_with('{') { continue; }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(text) = v["result"]["content"][0]["text"].as_str() {
            return Ok(text.to_string());
        }
    }
    Err(format!("{name}: no content returned"))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_generate_daily_brief(args: Value) -> Result<Value, String> {
    let sections_str = args.get("sections").and_then(Value::as_str)
        .unwrap_or("market,portfolio,ml,ops");
    let sections: Vec<&str> = sections_str.split(',').map(str::trim).collect();
    let date = args.get("date").and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());

    let mut brief_parts: Vec<String> = Vec::new();
    brief_parts.push(format!("# Daily Ops Brief — {date}\n"));
    let mut errors: Vec<String> = Vec::new();

    // Fetch sections concurrently.
    let mut tasks: Vec<(&str, tokio::task::JoinHandle<Result<String, String>>)> = Vec::new();

    if sections.contains(&"market") {
        tasks.push(("market", tokio::spawn(async {
            call_agora_tool("getMarketSnapshot", json!({})).await
        })));
    }
    if sections.contains(&"portfolio") {
        tasks.push(("portfolio", tokio::spawn(async {
            let pos = call_agora_tool("getOpenPositions", json!({})).await?;
            Ok(pos)
        })));
    }
    if sections.contains(&"ml") {
        tasks.push(("ml", tokio::spawn(async {
            call_agora_tool("getMlShadowStats", json!({})).await
        })));
    }
    if sections.contains(&"ops") {
        tasks.push(("ops", tokio::spawn(async {
            call_agora_tool("getSystemHealth", json!({})).await
        })));
    }

    for (section, handle) in tasks {
        match handle.await {
            Ok(Ok(text)) => {
                brief_parts.push(format!("## {}\n\n{}\n", section_title(section), text));
            }
            Ok(Err(e)) => errors.push(format!("{section}: {e}")),
            Err(e)     => errors.push(format!("{section}: join error: {e}")),
        }
    }

    if !errors.is_empty() {
        brief_parts.push(format!("\n---\n⚠️ Errors: {}", errors.join("; ")));
    }

    let content = brief_parts.join("\n");

    // Write to KB.
    let topic_key = format!("agora-daily-brief-{date}");
    let kb_status = try_kb_write_handoff(&content, &format!("daily-brief-{date}"), "sirin", "");

    Ok(json!({
        "date":       date,
        "sections":   sections,
        "topic_key":  topic_key,
        "kb_write":   kb_status,
        "errors":     errors,
        "brief":      content,
    }))
}

fn section_title(section: &str) -> &str {
    match section {
        "market"    => "Market Snapshot",
        "portfolio" => "Open Positions",
        "ml"        => "ML Shadow Stats",
        "ops"       => "System Health",
        other       => other,
    }
}

// ── #226 KB Merge ─────────────────────────────────────────────────────────────

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_kb_merge(args: Value) -> Result<Value, String> {
    let src_keys_str = args["src_keys"].as_str().ok_or("Missing src_keys")?;
    let dst_key      = args["dst_key"].as_str().ok_or("Missing dst_key")?;
    let project      = args.get("project").and_then(Value::as_str).unwrap_or("sirin");
    let strategy     = args.get("strategy").and_then(Value::as_str).unwrap_or("concat");
    let dry_run      = args.get("dry_run").and_then(Value::as_bool).unwrap_or(false);

    let src_keys: Vec<&str> = src_keys_str.split(',').map(str::trim).collect();

    // Fetch all source entries concurrently.
    let mut fetch_handles = Vec::new();
    for key in &src_keys {
        let k = key.to_string();
        let p = project.to_string();
        fetch_handles.push(tokio::spawn(async move {
            kb_get_via_http(&k, &p).await.map(|c| (k, c))
        }));
    }

    let mut src_contents: Vec<(String, String)> = Vec::new();
    let mut fetch_errors: Vec<String> = Vec::new();
    for handle in fetch_handles {
        match handle.await {
            Ok(Ok((k, c)))  => src_contents.push((k, c)),
            Ok(Err(e))      => fetch_errors.push(e),
            Err(e)          => fetch_errors.push(e.to_string()),
        }
    }

    if src_contents.is_empty() {
        return Err(format!("No source entries fetched. Errors: {}", fetch_errors.join("; ")));
    }

    // Merge content.
    let merged_content = match strategy {
        "llm" => {
            // Ask LLM to intelligently merge.
            let combined: String = src_contents.iter()
                .map(|(k, c)| format!("### [{k}]\n{c}"))
                .collect::<Vec<_>>().join("\n\n---\n\n");
            let merge_prompt = format!(
                "Merge these KB entries into one coherent, deduplicated entry. \
                 Preserve all unique information. Use markdown. Be concise (< 2000 chars):\n\n{combined}"
            );
            let ctx = crate::adk::context::AgentContext::new("kb_merge", crate::adk::tool::ToolRegistry::new());
            crate::llm::call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), merge_prompt)
                .await
                .map_err(|e| e.to_string())?
        }
        _ => {
            // concat: join with separators.
            src_contents.iter()
                .map(|(k, c)| format!("<!-- merged from {k} -->\n{c}"))
                .collect::<Vec<_>>().join("\n\n---\n\n")
        }
    };

    if dry_run {
        return Ok(json!({
            "dry_run":    true,
            "dst_key":    dst_key,
            "project":    project,
            "strategy":   strategy,
            "src_count":  src_contents.len(),
            "merged_len": merged_content.len(),
            "preview":    merged_content.chars().take(500).collect::<String>(),
        }));
    }

    // Get existing dst content (for append) or start fresh.
    let final_content = match kb_get_via_http(dst_key, project).await {
        Ok(existing) if !existing.is_empty() => {
            format!("{existing}\n\n---\n\n<!-- appended merge -->\n{merged_content}")
        }
        _ => merged_content.clone(),
    };

    // Write merged content to dst.
    let write_status = try_kb_write_handoff(&final_content, &format!("merge:{}", src_keys_str), project, "");

    // Mark sources as stale via best-effort HTTP calls.
    let mut stale_results: Vec<String> = Vec::new();
    for (key, _) in &src_contents {
        let k = key.clone();
        let p = project.to_string();
        let r = call_agora_tool("kbMarkStale", json!({ "topicKey": k, "project": p })).await;
        stale_results.push(format!("{k}: {}", if r.is_ok() { "stale" } else { "stale-failed" }));
    }

    Ok(json!({
        "dst_key":       dst_key,
        "project":       project,
        "strategy":      strategy,
        "src_count":     src_contents.len(),
        "merged_len":    final_content.len(),
        "kb_write":      write_status,
        "stale_results": stale_results,
        "fetch_errors":  fetch_errors,
    }))
}

// ── Saved Scripts (deterministic replay) ─────────────────────────────────────

pub(crate) fn call_list_saved_scripts() -> Result<Value, String> {
    use crate::test_runner::store;
    let tests = store::all_test_stats();
    let mut scripts: Vec<Value> = Vec::new();
    for t in &tests {
        if let Some((saved_at, success, fail)) = store::script_info(&t.test_id) {
            // Count actions without viewport check (use 365-day window)
            let action_count = store::load_script(&t.test_id, 365)
                .map(|a| a.len())
                .unwrap_or(0);
            // Viewport info (#135, #138)
            let recorded_vp = store::script_viewport(&t.test_id)
                .unwrap_or_else(|| "unknown".to_string());
            // Compare with YAML's current viewport
            let yaml_vp = crate::test_runner::parser::find(&t.test_id)
                .and_then(|tg| tg.viewport.map(|v| format!("{}x{}:{:.1}:{}", v.width, v.height, v.scale,
                    if v.mobile { "mobile" } else { "desktop" })))
                .unwrap_or_else(|| "default".to_string());
            let vp_match = recorded_vp == yaml_vp || recorded_vp == "unknown";
            let mut entry = json!({
                "test_id":           t.test_id,
                "saved_at":          saved_at,
                "success_count":     success,
                "fail_count":        fail,
                "action_count":      action_count,
                "pass_rate":         if success + fail > 0 {
                    success as f64 / (success + fail) as f64
                } else { 0.0 },
                "recorded_viewport": recorded_vp,
                "current_viewport":  yaml_vp,
                "viewport_ok":       vp_match,
            });
            if !vp_match {
                entry["warning"] = json!(
                    "⚠️ viewport mismatch — script will be auto-deleted on next run and regenerated"
                );
            }
            scripts.push(entry);
        }
    }
    scripts.sort_by(|a, b| {
        b.get("success_count").and_then(Value::as_u64).unwrap_or(0)
            .cmp(&a.get("success_count").and_then(Value::as_u64).unwrap_or(0))
    });
    Ok(json!({
        "count":   scripts.len(),
        "scripts": scripts,
        "note":    "Scripts are used for deterministic replay (0 LLM calls). Delete stale scripts when UI changes."
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_delete_saved_script(args: Value) -> Result<Value, String> {
    let test_id = args.get("test_id").and_then(Value::as_str)
        .ok_or("'delete_saved_script' requires 'test_id'")?;
    crate::test_runner::store::delete_script(test_id);
    Ok(json!({
        "status":  "deleted",
        "test_id": test_id,
        "note":    "Next run will use LLM ReAct loop and regenerate the script on success."
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_list_fixes(args: Value) -> Result<Value, String> {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20).min(100) as usize;
    let test_id = args.get("test_id").and_then(Value::as_str);

    let fixes = match test_id {
        Some(tid) => crate::test_runner::store::recent_fixes(tid, limit),
        None      => crate::test_runner::store::recent_fixes_all(limit),
    };
    let items: Vec<Value> = fixes.into_iter().map(|f| json!({
        "id":                  f.id,
        "test_id":             f.test_id,
        "run_id":              f.run_id,
        "category":            f.category,
        "triggered_at":        f.triggered_at,
        "completed_at":        f.completed_at,
        "outcome":             f.outcome,
        "claude_exit_code":    f.claude_exit_code,
        "claude_output":       f.claude_output,
        "verification_run_id": f.verification_run_id,
        "verified_at":         f.verified_at,
    })).collect();
    Ok(json!({ "count": items.len(), "fixes": items }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_config_diagnostics() -> Result<Value, String> {
    let issues = crate::config_check::run_diagnostics();
    let items: Vec<Value> = issues.iter().map(|i| json!({
        "severity":   match i.severity {
            crate::config_check::Severity::Ok      => "ok",
            crate::config_check::Severity::Info    => "info",
            crate::config_check::Severity::Warning => "warning",
            crate::config_check::Severity::Error   => "error",
        },
        "category":   i.category,
        "message":    i.message,
        "suggestion": i.suggestion,
    })).collect();
    let summary = crate::config_check::format_report(&issues);
    Ok(json!({
        "count": items.len(),
        "errors":   issues.iter().filter(|i| matches!(i.severity, crate::config_check::Severity::Error)).count(),
        "warnings": issues.iter().filter(|i| matches!(i.severity, crate::config_check::Severity::Warning)).count(),
        "ok":       issues.iter().filter(|i| matches!(i.severity, crate::config_check::Severity::Ok)).count(),
        "issues":   items,
        "text_report": summary,
    }))
}

// ── multi_agent MCP handlers ──────────────────────────────────────────────────

fn resolve_cwd(args: &Value) -> String {
    args["cwd"].as_str()
        .map(|s| s.to_string())
        .or_else(|| crate::claude_session::repo_path("sirin"))
        .unwrap_or_else(|| ".".to_string())
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_team_status(args: Value) -> Result<Value, String> {
    let cwd = resolve_cwd(&args);
    let guard = crate::multi_agent::get_or_init(&cwd);
    let team  = guard.as_ref().ok_or("team not initialized")?;
    Ok(serde_json::to_value(team.status()).unwrap_or(serde_json::json!({})))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_team_task(args: Value) -> Result<Value, String> {
    let task = args["task"].as_str().ok_or("Missing 'task'")?;
    let cwd  = resolve_cwd(&args);

    if !crate::claude_session::cli_available() {
        return Err("Claude CLI not available".into());
    }

    let mut guard = crate::multi_agent::get_or_init(&cwd);
    let team = guard.as_mut().ok_or("team not initialized")?;

    let review = team.assign_task(task)?;
    let status = team.status();
    Ok(serde_json::json!({
        "pm_review": review,
        "sessions":  serde_json::to_value(status).unwrap_or_default(),
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_team_test(args: Value) -> Result<Value, String> {
    let cwd = resolve_cwd(&args);

    if !crate::claude_session::cli_available() {
        return Err("Claude CLI not available".into());
    }

    let mut guard = crate::multi_agent::get_or_init(&cwd);
    let team = guard.as_mut().ok_or("team not initialized")?;

    let result = team.test_cycle()?;
    let status = team.status();
    Ok(serde_json::json!({
        "test_result": result,
        "sessions":    serde_json::to_value(status).unwrap_or_default(),
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_send(args: Value) -> Result<Value, String> {
    let role    = args["role"].as_str().ok_or("Missing 'role'")?;
    let message = args["message"].as_str().ok_or("Missing 'message'")?;
    let cwd     = resolve_cwd(&args);

    if !crate::claude_session::cli_available() {
        return Err("Claude CLI not available".into());
    }

    let mut guard = crate::multi_agent::get_or_init(&cwd);
    let team = guard.as_mut().ok_or("team not initialized")?;

    let reply = match role {
        "pm"       => team.pm.send(message)?,
        "engineer" => team.engineer.send(message)?,
        "tester"   => team.tester.send(message)?,
        other      => return Err(format!("Unknown role: {other}")),
    };

    let sid = match role {
        "pm"       => team.pm.session_id().map(|s| s.to_string()),
        "engineer" => team.engineer.session_id().map(|s| s.to_string()),
        "tester"   => team.tester.session_id().map(|s| s.to_string()),
        _          => None,
    };

    Ok(serde_json::json!({
        "role":       role,
        "reply":      reply,
        "session_id": sid,
        "resume_cmd": sid.as_deref().map(|id| format!("claude --resume {id}"))
                        .unwrap_or_else(|| "(no session yet)".into()),
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_reset(args: Value) -> Result<Value, String> {
    let role = args["role"].as_str().ok_or("Missing 'role'")?;
    let cwd  = resolve_cwd(&args);

    let mut guard = crate::multi_agent::get_or_init(&cwd);
    let team = guard.as_mut().ok_or("team not initialized")?;

    if role == "all" {
        team.reset_role("pm");
        team.reset_role("engineer");
        team.reset_role("tester");
    } else {
        team.reset_role(role);
    }

    Ok(serde_json::json!({ "reset": role, "status": "ok" }))
}

// ── 任務佇列 + Worker ─────────────────────────────────────────────────────────

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_enqueue(args: Value) -> Result<Value, String> {
    let task = args["task"].as_str().ok_or("Missing 'task'")?;
    let priority = args.get("priority")
        .and_then(|v| v.as_u64())
        .map(|n| n.min(255) as u8)
        .unwrap_or(50);

    // T2-2: optional yaml_test_id triggers YAML verification after Engineer completes.
    let yaml_test_id: Option<String> = args.get("yaml_test_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from);

    let id = if let Some(ytid) = yaml_test_id.as_deref() {
        let ctx = crate::multi_agent::queue::ProjectContext {
            yaml_test_id: Some(ytid.to_string()),
            ..Default::default()
        };
        crate::multi_agent::queue::enqueue_with_project(task, priority, ctx)
    } else {
        crate::multi_agent::queue::enqueue_with_priority(task, priority)
    };

    tracing::info!(target: "sirin",
        "[mcp] agent_enqueue: task_id={id} priority={priority} yaml_test_id={:?} task={:.60}",
        yaml_test_id, task);

    let msg = if yaml_test_id.is_some() {
        format!("任務已加入佇列（T2-2：完成後自動驗證 YAML test '{}'）。\
                 用 agent_start_worker 確保 Worker 正在執行，用 agent_queue_status 查詢進度。",
            yaml_test_id.as_deref().unwrap_or(""))
    } else {
        "任務已加入佇列。用 agent_start_worker 確保 Worker 正在執行，\
         用 agent_queue_status 查詢進度。".to_string()
    };

    Ok(serde_json::json!({
        "task_id":      id,
        "status":       "queued",
        "yaml_test_id": yaml_test_id,
        "message":      msg,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_queue_status() -> Result<Value, String> {
    let tasks = crate::multi_agent::queue::list_all();

    // 安全截斷輔助：找 max_bytes 內最後一個 char boundary
    fn safe_truncate(s: &str, max_bytes: usize) -> &str {
        let end = s.len().min(max_bytes);
        let boundary = (0..=end).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        &s[..boundary]
    }

    let summary: Vec<_> = tasks.iter().map(|t| serde_json::json!({
        "id":          t.id,
        "status":      t.status.to_string(),
        "description": safe_truncate(&t.description, 80),
        "created_at":  t.created_at,
        "finished_at": t.finished_at,
        "result_preview": t.result.as_deref()
            .map(|r| safe_truncate(r, 120).to_string()),
    })).collect();
    Ok(serde_json::json!({
        "total":   tasks.len(),
        "queued":  tasks.iter().filter(|t| t.status == crate::multi_agent::TaskStatus::Queued).count(),
        "running": tasks.iter().filter(|t| t.status == crate::multi_agent::TaskStatus::Running).count(),
        "done":    tasks.iter().filter(|t| t.status == crate::multi_agent::TaskStatus::Done).count(),
        "failed":  tasks.iter().filter(|t| t.status == crate::multi_agent::TaskStatus::Failed).count(),
        "tasks":   summary,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_start_worker(args: Value) -> Result<Value, String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);

    if STARTED.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        let cwd = resolve_cwd(&args);
        // T1-1: optional `n` for parallel workers (default 1, capped at 8 to
        // protect Anthropic API rate limit).
        let n = args.get("n")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(1)
            .clamp(1, 8);
        crate::multi_agent::worker::spawn_n(&cwd, n);
        Ok(serde_json::json!({ "status": "started", "cwd": cwd, "workers": n }))
    } else {
        Ok(serde_json::json!({ "status": "already_running" }))
    }
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_agent_clear_completed() -> Result<Value, String> {
    let before = crate::multi_agent::queue::list_all().len();
    crate::multi_agent::queue::clear_completed();
    let after = crate::multi_agent::queue::list_all().len();
    Ok(serde_json::json!({
        "removed": before - after,
        "remaining": after,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_squad_knowledge(arguments: Value) -> Result<Value, String> {
    let limit = arguments["limit"].as_u64().unwrap_or(20).min(100) as usize;
    let lessons = crate::multi_agent::knowledge::all_lessons(limit);
    let total   = crate::multi_agent::knowledge::lesson_count();
    Ok(serde_json::json!({
        "total": total,
        "showing": lessons.len(),
        "lessons": lessons.iter().map(|(key, value, learned_at)| serde_json::json!({
            "key":        key,
            "lesson":     value,
            "learned_at": learned_at,
        })).collect::<Vec<_>>(),
    }))
}

// ── dev_team_* (GitHub issue ↔ dev team bridge) ──────────────────────────────
//
// Thin MCP-facing wrappers around `multi_agent::github_adapter`. Default to
// dry_run=true on every enqueue so external Claude sessions can experiment
// without writing to GitHub or pushing commits — they have to explicitly
// opt out by sending `dry_run: false`.

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_dev_team_enqueue_issue(args: Value) -> Result<Value, String> {
    let project_key  = args["project_key"].as_str()
        .ok_or("Missing 'project_key' (e.g. 'agora_market', 'sirin')")?;
    let gh_repo      = args["gh_repo"].as_str()
        .ok_or("Missing 'gh_repo' (e.g. 'Redandan/AgoraMarket')")?;
    let issue_number = args["issue_number"].as_u64()
        .ok_or("Missing 'issue_number' (positive integer)")? as u32;
    // Default dry_run=true — safer for external callers; they must explicitly
    // opt in to live mode.
    let dry_run  = args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(true);
    let priority = args.get("priority").and_then(|v| v.as_u64())
        .map(|n| n.min(255) as u8).unwrap_or(50);

    let task_id = if dry_run {
        crate::multi_agent::github_adapter::enqueue_from_issue_dry_run(
            project_key, gh_repo, issue_number,
        )?
    } else {
        crate::multi_agent::github_adapter::enqueue_from_issue_with_priority(
            project_key, gh_repo, issue_number, priority,
        )?
    };

    tracing::info!(target: "sirin",
        "[mcp] dev_team_enqueue_issue: project={project_key} repo={gh_repo} \
         issue=#{issue_number} dry_run={dry_run} task_id={task_id}");

    let issue_url = format!("https://github.com/{gh_repo}/issues/{issue_number}");
    let next_step = if dry_run {
        "Dev team 會以 DRY-RUN 模式處理（不貼 GitHub、不 git push）；完成後 \
         用 dev_team_list_previews 看結果，OK 的話 dev_team_replay_preview 真的貼"
    } else {
        "Dev team 會正常處理；完成後 system 自動把 PM 的 review 貼回 issue 留言"
    };
    Ok(serde_json::json!({
        "task_id":   task_id,
        "status":    "queued",
        "dry_run":   dry_run,
        "issue_url": issue_url,
        "message":   next_step,
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_dev_team_list_previews(args: Value) -> Result<Value, String> {
    let limit = args.get("limit").and_then(|v| v.as_u64())
        .map(|n| n.min(200) as usize).unwrap_or(20);
    let issue_url_filter = args.get("issue_url").and_then(|v| v.as_str()).map(|s| s.to_string());

    // Helper: char-boundary-safe truncation
    fn safe_truncate(s: &str, max_bytes: usize) -> String {
        if s.len() <= max_bytes { return s.to_string(); }
        let end = (0..=max_bytes).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        format!("{}…", &s[..end])
    }

    // Newest first; filter; cap at limit.
    let mut previews = crate::multi_agent::github_adapter::list_preview_comments();
    previews.reverse();
    if let Some(url) = &issue_url_filter {
        previews.retain(|p| p.issue_url == *url);
    }
    previews.truncate(limit);

    let summary: Vec<_> = previews.iter().map(|p| serde_json::json!({
        "task_id":      p.task_id,
        "issue_url":    p.issue_url,
        "success":      p.success,
        "saved_at":     p.saved_at,
        "body_preview": safe_truncate(&p.body, 200),
        "body_chars":   p.body.len(),
    })).collect();

    Ok(serde_json::json!({
        "total":    summary.len(),
        "previews": summary,
        "hint":     "Use dev_team_replay_preview with task_id to actually post one to GitHub.",
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_dev_team_replay_preview(args: Value) -> Result<Value, String> {
    let task_id = args["task_id"].as_str()
        .ok_or("Missing 'task_id' (from dev_team_list_previews)")?;

    let preview = crate::multi_agent::github_adapter::latest_preview_for(task_id)
        .ok_or_else(|| format!(
            "No preview found for task_id '{task_id}'. \
             Use dev_team_list_previews to see available task_ids."))?;

    crate::multi_agent::github_adapter::replay_preview(&preview)?;

    tracing::info!(target: "sirin",
        "[mcp] dev_team_replay_preview: task_id={task_id} → {}", preview.issue_url);

    Ok(serde_json::json!({
        "status":    "posted",
        "task_id":   task_id,
        "issue_url": preview.issue_url,
        "message":   "Comment posted to GitHub. Preview record kept on disk for audit.",
    }))
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_dev_team_read_issue(args: Value) -> Result<Value, String> {
    let gh_repo      = args["gh_repo"].as_str()
        .ok_or("Missing 'gh_repo' (e.g. 'Redandan/AgoraMarket')")?;
    let issue_number = args["issue_number"].as_u64()
        .ok_or("Missing 'issue_number'")? as u32;

    let issue = crate::multi_agent::github_adapter::read_issue(gh_repo, issue_number)?;
    Ok(serde_json::json!({
        "title":  issue.title,
        "body":   issue.body,
        "labels": issue.labels,
        "url":    format!("https://github.com/{gh_repo}/issues/{issue_number}"),
    }))
}

// ── consult / supervised_run ──────────────────────────────────────────────────

/// 把問題轉給另一個 Claude session，帶回建議。
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_consult(args: Value) -> Result<Value, String> {
    use crate::claude_session;

    let question = args["question"].as_str().ok_or("Missing 'question'")?;
    let context  = args["context"].as_str().unwrap_or("");
    let cwd = args["cwd"].as_str()
        .map(|s| s.to_string())
        .or_else(|| claude_session::repo_path("sirin"))
        .ok_or("Missing 'cwd' and sirin repo path not found")?;

    if !claude_session::cli_available() {
        return Err("Claude CLI not available — install with: npm install -g @anthropic-ai/claude-code".into());
    }

    let advice = claude_session::consult(question, context, &cwd)?;
    Ok(json!({
        "advice": advice,
        "consultant_cwd": cwd,
    }))
}

/// 自然語言助理任務 — 視覺驅動的 ReAct loop（非同步，立即回傳 run_id）。
/// 用 get_test_result 輪詢狀態，完成後 analysis 欄位包含結果。
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_assistant_task(args: Value) -> Result<Value, String> {
    let request = args.get("request").and_then(Value::as_str)
        .ok_or("'assistant_task' requires 'request'")?
        .to_string();
    let url = args.get("url").and_then(Value::as_str).map(String::from);

    tracing::info!("[assistant] queuing task: {}", &request[..request.len().min(80)]);

    // Use the test runner's run registry for polling compatibility.
    let run_id = crate::test_runner::runs::new_run("assistant");
    let run_id_clone = run_id.clone();
    let request_clone = request.clone();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                crate::test_runner::runs::set_phase(
                    &run_id_clone,
                    crate::test_runner::runs::RunPhase::Error(format!("runtime: {e}"))
                );
                return;
            }
        };
        rt.block_on(async {
            let _guard = crate::test_runner::TEST_RUN_LOCK.lock().await;
            let tools = crate::adk::tool::default_tool_registry();
            let ctx = crate::adk::context::AgentContext::new("assistant", tools);
            let result = crate::assistant::run_task(&ctx, &request_clone, url.as_deref()).await;

            // Encode result as a synthetic TestResult so get_test_result works.
            let tr = crate::test_runner::executor::TestResult {
                test_id: "assistant".to_string(),
                status: if result.success {
                    crate::test_runner::executor::TestStatus::Passed
                } else {
                    crate::test_runner::executor::TestStatus::Failed
                },
                iterations: result.steps,
                duration_ms: 0,
                error_message: if result.success { None } else {
                    Some(result.summary.clone())
                },
                // Issue #144: save final screenshot so get_test_result can surface it.
                screenshot_path: result.screenshot_b64.as_deref().and_then(|b64| {
                    let failures_dir = crate::platform::app_data_dir().join("test_failures");
                    let path = failures_dir.join(format!(
                        "assistant_{}.png",
                        chrono::Local::now().format("%Y%m%d_%H%M%S")
                    ));
                    let _ = std::fs::create_dir_all(&failures_dir);
                    let bytes = base64_decode(b64)?;
                    std::fs::write(&path, &bytes).ok()?;
                    Some(path.to_string_lossy().to_string())
                }),
                screenshot_error: None,
                history: vec![],
                final_analysis: Some(format!(
                    "{}\n\ndata: {}",
                    result.summary,
                    result.data.as_ref()
                        .map(|d| d.to_string())
                        .unwrap_or_default()
                )),
                dispute: None,
            };
            crate::test_runner::runs::set_phase(
                &run_id_clone,
                crate::test_runner::runs::RunPhase::Complete(tr)
            );
        });
    });

    Ok(json!({
        "run_id":       run_id,
        "status":       "queued",
        "poll_with":    "get_test_result",
        "request":      request,
    }))
}

/// 以受監督模式執行 Claude Code session。
/// 遇到停頓時根據 policy 自動回應（auto=yes；consult=問另一個 session）。
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_supervised_run(args: Value) -> Result<Value, String> {
    use crate::claude_session::{self, SupervisionPolicy, SupervisionEvent};

    let cwd    = args["cwd"].as_str().ok_or("Missing 'cwd'")?;
    let prompt = args["prompt"].as_str().ok_or("Missing 'prompt'")?;

    if !claude_session::cli_available() {
        return Err("Claude CLI not available — install with: npm install -g @anthropic-ai/claude-code".into());
    }

    let policy = match args["policy"].as_str().unwrap_or("auto") {
        "consult" => SupervisionPolicy::Consult {
            consultant_cwd: args["consultant_cwd"].as_str().map(|s| s.to_string()),
        },
        _ => SupervisionPolicy::AutoApprove,
    };

    // Collect events for the summary response
    let events: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    let result = claude_session::run_supervised(cwd, prompt, &policy, &|event| {
        let line = match &event {
            SupervisionEvent::Working    { text }   => {
                let e = text.len().min(120);
                let e = (0..=e).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
                format!("working: {}", &text[..e])
            },
            SupervisionEvent::UsingTool  { name }     => format!("tool: {name}"),
            SupervisionEvent::Paused     { question } => format!("paused: {question}"),
            SupervisionEvent::Consulting { question } => format!("consulting: {question}"),
            SupervisionEvent::GotAdvice  { advice }   => {
                let e = advice.len().min(200);
                let e = (0..=e).rev().find(|&i| advice.is_char_boundary(i)).unwrap_or(0);
                format!("advice: {}", &advice[..e])
            },
            SupervisionEvent::Continuing { round }    => format!("continuing round {round}"),
            SupervisionEvent::Done       { .. }       => "done".into(),
        };
        events.lock().unwrap_or_else(|e| e.into_inner()).push(line);
    });

    let event_log = events.into_inner().unwrap_or_default();

    match result {
        Ok(r) => Ok(json!({
            "success":    r.success,
            "exit_code":  r.exit_code,
            "output":     r.output,
            "rounds":     event_log.iter().filter(|e| e.starts_with("continuing")).count() + 1,
            "event_log":  event_log,
        })),
        Err(e) => Ok(json!({
            "success":   false,
            "error":     e,
            "event_log": event_log,
        })),
    }
}

async fn call_browser_exec(args: Value, user_agent: &str) -> Result<Value, String> {
    // ── AuthZ gate ────────────────────────────────────────────────────────────
    let action_name = args["action"].as_str().unwrap_or("").to_string();
    // Resolve per-request — never read a global.  Concurrent clients each
    // carry their own UA, so the session lookup is race-free.
    let client_id   = resolve_client_id(user_agent);
    let current_url = crate::browser::current_url().ok();
    let cfg         = crate::authz::global_config();
    let decision    = crate::authz::decide(&client_id, &action_name, &args, &current_url, &cfg);
    match &decision {
        crate::authz::Decision::Allow(reason) => {
            crate::authz::audit::log_allow(
                &cfg.audit.log_path, &client_id, &action_name, &args, &current_url, reason,
            );
        }
        crate::authz::Decision::Deny(reason) => {
            crate::authz::audit::log_deny(
                &cfg.audit.log_path, &client_id, &action_name, &args, &current_url, reason,
            );
            return Err(format!("authz denied: {reason}"));
        }
        crate::authz::Decision::Ask(reason) => {
            crate::authz::audit::log_ask(
                &cfg.audit.log_path, &client_id, &action_name, &args, &current_url, reason,
            );
            let req_id = format!("ask-{}-{}", &action_name, uuid_v4_short());
            crate::monitor::emit_authz_ask(
                &req_id, &client_id, &action_name, args.clone(),
                current_url.as_deref().unwrap_or(""),
                30_000, // timeout_ms
                false,  // learn: false
            ).await;
            // Wait for human decision (30s timeout → deny)
            if let Some(ms) = crate::monitor::state() {
                let rx = ms.register_authz_ask(&req_id);
                match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                    Ok(Ok(crate::monitor::AuthzDecisionResult::Allow)) => {
                        // User clicked Allow — continue execution
                    }
                    Ok(Ok(crate::monitor::AuthzDecisionResult::Deny)) | Ok(Err(_)) | Err(_) => {
                        return Err(format!("authz ask denied by operator (or timed out): {reason}"));
                    }
                }
            } else {
                return Err(format!("authz ask (no monitor GUI): {reason}"));
            }
        }
        crate::authz::Decision::AskWithLearn => {
            crate::authz::audit::log_ask(
                &cfg.audit.log_path, &client_id, &action_name, &args, &current_url, "ask+learn",
            );
            let req_id = format!("ask-{}-{}", &action_name, uuid_v4_short());
            crate::monitor::emit_authz_ask(
                &req_id, &client_id, &action_name, args.clone(),
                current_url.as_deref().unwrap_or(""),
                30_000, // timeout_ms
                true,   // learn: true
            ).await;
            // Wait for human decision (30s timeout → deny)
            if let Some(ms) = crate::monitor::state() {
                let rx = ms.register_authz_ask(&req_id);
                match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                    Ok(Ok(crate::monitor::AuthzDecisionResult::Allow)) => {
                        // User clicked Allow — continue execution
                    }
                    Ok(Ok(crate::monitor::AuthzDecisionResult::Deny)) | Ok(Err(_)) | Err(_) => {
                        return Err("authz ask denied by operator (or timed out): ask+learn".to_string());
                    }
                }
            } else {
                return Err("authz ask (no monitor GUI): ask+learn".to_string());
            }
        }
    }

    // ── Control gate (Pause / Step / Abort) ──────────────────────────────────
    crate::monitor::control().gate().await
        .map_err(|e| format!("control: {e}"))?;

    // ── Monitor emit ──────────────────────────────────────────────────────────
    let action_id = format!("{}-{}", &action_name, uuid_v4_short());
    crate::monitor::emit_action_start(&client_id, &action_id, &action_name, args.clone()).await;
    let t0 = std::time::Instant::now();

    let action = args["action"].as_str().ok_or("Missing action")?.to_string();
    // target / text / timeout are kept for the screenshot_analyze / ocr_find_text
    // async-only handlers below; all other parameter parsing is done inside
    // browser_exec::dispatch() which reads them from the raw `args` Value.
    let target  = args.get("target").and_then(Value::as_str).unwrap_or("").to_string();
    let timeout = args.get("timeout").and_then(Value::as_u64);
    let headless_override = args.get("browser_headless").and_then(Value::as_bool);
    // session_id pre-processing happens inside the blocking closure below.
    let session_id_arg = args.get("session_id").and_then(Value::as_str).map(String::from);

    // ── Async-only actions (need LLM call, can't go in spawn_blocking) ────
    if action == "screenshot_analyze" {
        if target.is_empty() {
            let e = "'screenshot_analyze' requires 'target' = analysis prompt".to_string();
            crate::monitor::emit_action_error(&action_id, &e).await;
            return Err(e);
        }
        // Ensure browser open in correct mode first (might trigger vision-needing
        // re-launch for Flutter/WebGL).
        let want_headless = headless_override.unwrap_or_else(crate::browser::default_headless);
        blocking("ensure_open", move || crate::browser::ensure_open(want_headless))
            .await?;
        let llm = crate::llm::shared_llm();
        let client = crate::llm::shared_http();
        match crate::llm::analyze_screenshot(&client, &llm, &target).await {
            Ok(analysis) => {
                let dur = t0.elapsed().as_millis() as u64;
                crate::monitor::emit_action_done(&action_id, json!({"analysis": &analysis}), dur).await;
                return Ok(json!({ "analysis": analysis, "prompt": target }));
            }
            Err(e) => {
                let msg = e.to_string();
                crate::monitor::emit_action_error(&action_id, &msg).await;
                return Err(format!("vision analysis failed: {msg}"));
            }
        }
    }

    if action == "ocr_find_text" {
        if target.is_empty() {
            let e = "'ocr_find_text' requires 'target' = text to find".to_string();
            crate::monitor::emit_action_error(&action_id, &e).await;
            return Err(e);
        }
        let max_results = timeout.unwrap_or(5) as usize;
        let result = blocking("ocr_find_text", move || {
            crate::perception::ocr::find_text_on_current_page(&target, max_results)
        }).await;
        match result {
            Ok(val) => {
                let dur = t0.elapsed().as_millis() as u64;
                crate::monitor::emit_action_done(&action_id, val.clone(), dur).await;
                return Ok(val);
            }
            Err(e) => {
                crate::monitor::emit_action_error(&action_id, &e).await;
                return Err(format!("local OCR failed: {e}"));
            }
        }
    }

    // Dispatch via the shared browser_exec module.  All common browser actions
    // live there; only MCP-specific ext_* probes are handled inline below.
    let result = blocking("browser_exec_sync", move || -> Result<Value, String> {
        use crate::browser;
        let want_headless = headless_override.unwrap_or_else(browser::default_headless);

        // ── Session switching (Issue #19 P1) ─────────────────────────────
        // For goto with session_id: ensure browser open with correct headless
        // mode BEFORE switching, so the new tab is launched correctly.
        if let Some(ref sid) = session_id_arg {
            if action.as_str() == "goto" {
                browser::ensure_open(want_headless)?;
            }
            browser::session_switch(sid)?;
        }

        // ── Sirin Companion extension probes (MCP-only, not in browser_exec) ──
        match action.as_str() {
            "ext_status" => {
                return serde_json::to_value(crate::ext_server::status())
                    .map_err(|e| format!("ext_status serialize: {e}"));
            }
            "ext_url" => {
                // Authoritative URL from extension; falls back to CDP cache.
                let tab_id = args.get("tab_id").and_then(Value::as_i64);
                return match crate::ext_server::authoritative_url(tab_id) {
                    Some(u) => Ok(json!({ "url": u, "source": "extension" })),
                    None    => Ok(json!({
                        "url":    browser::current_url().unwrap_or_default(),
                        "source": "cdp_cache_fallback",
                    })),
                };
            }
            "ext_tabs" => {
                return Ok(json!({ "tabs": crate::ext_server::list_tabs() }));
            }
            _ => {}
        }

        // ── All other actions: shared dispatch ────────────────────────────
        crate::browser_exec::dispatch(action.as_str(), &args)
    })
    .await;

    let dur = t0.elapsed().as_millis() as u64;
    match &result {
        Ok(v)  => crate::monitor::emit_action_done(&action_id, v.clone(), dur).await,
        Err(e) => crate::monitor::emit_action_error(&action_id, e).await,
    }
    result
}

/// MCP `browser_status` — 列出目前 Chrome 所有開啟的 tab 和 named session，方便排查。
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
// ── Phase A diagnostics: tail_app_log + queue_status ────────────────────────
//
// Both handlers wrap data that the runtime already has but never exposed
// over MCP.  Added 2026-05-05 after a debug session where we saw "49 queued
// from 10:52" in the snapshot and couldn't tell whether tests were actually
// stuck or just waiting for the concurrency-1 lock.

/// Return up to `lines` (default 100, max 300) recent log lines from the
/// in-process ring buffer that `LogBufferLayer` populates from every
/// `tracing::info/warn/error!` call.
///
/// Args:
///   - `lines`: usize, default 100, clamped to [1, 300]
///   - `grep`:  optional substring filter (case-sensitive); only matching
///     lines are returned
///   - `level`: optional level filter — one of "ERROR", "WARN", "INFO".
///     Tracing's fmt layer prefixes lines like ` INFO [target]` /
///     ` WARN [target]` so we substring-match on the level token.
///
/// Returned shape:
///   { lines: [...], count: N, total_buffered: M, version: V, filter: {...} }
pub(crate) fn call_tail_app_log(args: Value) -> Result<Value, String> {
    let lines_req = args.get("lines")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 300) as usize;
    let grep = args.get("grep").and_then(Value::as_str).map(str::to_string);
    let level = args.get("level").and_then(Value::as_str).map(str::to_uppercase);

    // Pull more than asked when filtering, so the post-filter slice still
    // approximates `lines_req` lines.  Capped at MAX_LINES (300) anyway.
    let pull = if grep.is_some() || level.is_some() { 300 } else { lines_req };
    let raw = crate::log_buffer::recent(pull);

    let filtered: Vec<String> = raw.into_iter()
        .filter(|line| {
            let level_ok = match level.as_deref() {
                None => true,
                Some(want) => line.contains(want),
            };
            let grep_ok = match grep.as_deref() {
                None => true,
                Some(needle) => line.contains(needle),
            };
            level_ok && grep_ok
        })
        .collect();

    // Trim back to requested count (oldest dropped — most recent kept).
    let final_lines: Vec<String> = if filtered.len() > lines_req {
        filtered.into_iter().rev().take(lines_req).rev().collect()
    } else {
        filtered
    };

    Ok(json!({
        "lines":          final_lines.clone(),
        "count":          final_lines.len(),
        "total_buffered": crate::log_buffer::len(),
        "version":        crate::log_buffer::version(),
        "filter": {
            "grep":  grep,
            "level": level,
            "lines_requested": lines_req,
        },
    }))
}

/// Test-runner queue + concurrency snapshot.
///
/// Sirin serializes test execution via the process-wide
/// `test_runner::TEST_RUN_LOCK` (concurrency = 1), so a `run_test_batch`
/// of N tests appears in dashboards as N entries with the same
/// `started_at`, even though only one is actually running.  This handler
/// disambiguates: it shows running vs queued counts, the oldest queue
/// wait, and a drain estimate based on median test duration of the last
/// 50 completed runs.
///
/// Returned shape:
///   {
///     concurrency: 1,
///     running: { test_id, run_id, idle_secs, last_action_age_ms,
///                step, current_action } | null,
///     queued_count: N,
///     queued: [ { test_id, run_id, queued_for_secs }, ... ],   // up to 20
///     oldest_queued_for_secs: M,
///     median_test_secs_last_50: 32,
///     p95_test_secs_last_50:    280,
///     estimated_drain_secs:     queued_count × median_test_secs,
///     sampled_recent_runs:      50,
///   }
pub(crate) fn call_queue_status(_args: Value) -> Result<Value, String> {
    let now = chrono::Local::now();
    let active = crate::test_runner::runs::snapshot_active();

    // Bucket into running vs queued.  Concurrency-1 means at most one
    // entry in `running`; we still surface a Vec for forward compat in
    // case TEST_RUN_LOCK becomes optional in the future.
    let mut running_entries: Vec<Value> = Vec::new();
    let mut queued_entries:  Vec<(String, String, i64)> = Vec::new();  // (test_id, run_id, secs_waited)

    for state in &active {
        match &state.phase {
            crate::test_runner::runs::RunPhase::Running { step, current_action } => {
                let elapsed = state.last_phase_updated_at.elapsed();
                running_entries.push(json!({
                    "test_id":            state.test_id,
                    "run_id":             state.run_id,
                    "started_at":         state.started_at,
                    "idle_secs":          elapsed.as_secs(),
                    "last_action_age_ms": elapsed.as_millis() as u64,
                    "step":               step,
                    "current_action":     current_action,
                    "replay_mode":        state.replay_mode,
                }));
            }
            crate::test_runner::runs::RunPhase::Queued => {
                let waited = chrono::DateTime::parse_from_rfc3339(&state.started_at)
                    .map(|dt| (now.signed_duration_since(dt.with_timezone(&chrono::Local))).num_seconds())
                    .unwrap_or(0);
                queued_entries.push((state.test_id.clone(), state.run_id.clone(), waited));
            }
            _ => {}
        }
    }

    queued_entries.sort_by_key(|(_, _, w)| -*w);  // oldest first
    let oldest_queued = queued_entries.first().map(|(_, _, w)| *w).unwrap_or(0);
    let queued_count = queued_entries.len();

    // Compute median + p95 from the last 50 completed runs (across all
    // tests).  Ignores 0-duration runs — those are usually error-paths
    // that bailed before any work and would skew the median down.
    let recent = crate::test_runner::store::recent_runs_all(50);
    let mut durations: Vec<u64> = recent.iter()
        .filter_map(|r| r.duration_ms)
        .filter(|d| *d > 0)
        .map(|d| ((d / 1000) as u64).max(1))
        .collect();
    durations.sort_unstable();
    let median = if durations.is_empty() { 0 } else { durations[durations.len() / 2] };
    let p95 = if durations.is_empty() { 0 } else { durations[(durations.len() * 95 / 100).min(durations.len() - 1)] };
    let estimated_drain = (queued_count as u64) * median;

    let queued_sample: Vec<Value> = queued_entries.iter().take(20).map(|(test_id, run_id, w)| {
        json!({
            "test_id":         test_id,
            "run_id":          run_id,
            "queued_for_secs": w,
        })
    }).collect();

    Ok(json!({
        "concurrency":              1,
        "running":                  running_entries.first().cloned(),
        "queued_count":             queued_count,
        "queued":                   queued_sample,
        "oldest_queued_for_secs":   oldest_queued,
        "median_test_secs_last_50": median,
        "p95_test_secs_last_50":    p95,
        "estimated_drain_secs":     estimated_drain,
        "sampled_recent_runs":      durations.len(),
        "summary": format!(
            "{run} running, {q} queued — oldest queued {o}s, est. drain {drain}s (median {m}s × {q})",
            run = if running_entries.is_empty() { "0" } else { "1" },
            q = queued_count,
            o = oldest_queued,
            drain = estimated_drain,
            m = median,
        ),
    }))
}

/// Phase B — return the most recent N TestStep records for a running (or
/// just-completed) test, plus the current sub-phase + idle hints.
///
/// Until Phase B, `get_test_result` only exposed a one-line `current_action`
/// string for the in-flight step.  This handler surfaces the full step
/// history mirrored into RunState (`runs::push_step_summary`) so callers
/// can answer "what is the executor doing right now, and what did it just
/// finish doing?" without waiting for the run to terminate.
///
/// Args:
///   - `run_id` (required) — the spawned-run id from `run_test_async`
///   - `last_n` (optional, default 5, clamp 1..30) — how many recent
///     steps to return; oldest first
///
/// Returned shape:
///   {
///     run_id, test_id, status, idle_secs, last_action_age_ms,
///     current_subphase, subphase_age_ms, replay_mode,
///     recent_steps_count, returned_steps_count,
///     recent_steps: [
///       { iter, thought, action, observation, llm_model,
///         llm_latency_ms, parse_errors, ts },
///       ...
///     ]
///   }
pub(crate) fn call_live_trace(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?;
    let last_n = args.get("last_n")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 30) as usize;

    let state = crate::test_runner::runs::get(run_id)
        .ok_or_else(|| format!("run_id '{run_id}' not found (may have been pruned)"))?;
    let mut envelope = crate::test_runner::runs::to_json(&state);

    let steps = crate::test_runner::runs::recent_steps(run_id, last_n);
    let serialised: Vec<Value> = steps.iter().enumerate().map(|(i, step)| {
        // Action input can be huge for screenshot_analyze (base64 image
        // payload) or file_write — clip the inline preview but keep the
        // action `name` key intact so callers can still tell what was done.
        let action_preview = match &step.action {
            Value::Object(map) => {
                let mut clipped = serde_json::Map::new();
                for (k, v) in map {
                    let s = match v {
                        Value::String(text) if text.len() > 200 => {
                            Value::String(format!("{}…(truncated to 200)", &text[..200]))
                        }
                        other => other.clone(),
                    };
                    clipped.insert(k.clone(), s);
                }
                Value::Object(clipped)
            }
            other => other.clone(),
        };
        // Observation too — clip to 1KB so a wall of AX-tree dump doesn't
        // blow the MCP response budget.
        let obs_preview = if step.observation.len() > 1024 {
            format!("{}…(truncated to 1024 of {} chars; use get_full_observation for full)",
                &step.observation[..1024], step.observation.len())
        } else {
            step.observation.clone()
        };
        json!({
            "iter":               i,
            "thought":            step.thought,
            "action":             action_preview,
            "observation":        obs_preview,
            "observation_chars":  step.observation.len(),
            "llm_model":          step.llm_model,
            "llm_latency_ms":     step.llm_latency_ms,
            "llm_prompt_bytes":   step.llm_prompt_bytes,
            "llm_response_bytes": step.llm_response_bytes,
            "parse_errors":       step.parse_errors,
            "ts":                 step.ts,
        })
    }).collect();

    if let Some(obj) = envelope.as_object_mut() {
        obj.insert("returned_steps_count".into(), json!(serialised.len()));
        obj.insert("requested_last_n".into(), json!(last_n));
        obj.insert("recent_steps".into(), Value::Array(serialised));
    }
    Ok(envelope)
}

pub(crate) fn call_browser_status() -> Result<Value, String> {
    let status = crate::browser::browser_status();
    let is_open = status.is_open;
    let tab_count = status.tab_count;
    let active_idx = status.active_tab_index;

    // Build tabs array
    let tabs: Vec<Value> = status.tabs.into_iter().enumerate().map(|(i, url)| {
        let session = status.named_sessions.iter()
            .find(|(_, idx, _)| *idx == i)
            .map(|(sid, _, _)| sid.clone());
        let is_active = i == active_idx;
        let mut obj = json!({
            "index": i,
            "url": url,
            "active": is_active
        });
        if let Some(sid) = session {
            obj["session_id"] = json!(sid);
        }
        obj
    }).collect();

    let named: Vec<Value> = status.named_sessions.iter().map(|(sid, idx, url)| {
        json!({ "session_id": sid, "tab_index": idx, "url": url })
    }).collect();

    Ok(json!({
        "is_open": is_open,
        "tab_count": tab_count,
        "active_tab_index": active_idx,
        "tabs": tabs,
        "named_sessions": named,
        "summary": if is_open {
            format!("{} tab(s) open, {} named session(s)", tab_count, named.len())
        } else {
            "Chrome not running".into()
        }
    }))
}

// ── #187 sync_config ─────────────────────────────────────────────────────────

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_sync_config() -> Result<Value, String> {
    let repo_config = std::env::current_dir()
        .map_err(|e| format!("cwd: {e}"))?
        .join("config");
    let local_config = crate::platform::app_data_dir().join("config");

    let count = copy_dir_recursive(&repo_config, &local_config)
        .map_err(|e| format!("sync_config failed: {e}"))?;

    tracing::info!("[sync_config] synced {} files: {} → {}", count,
        repo_config.display(), local_config.display());

    Ok(json!({
        "synced": true,
        "files_copied": count,
        "src": repo_config.to_string_lossy(),
        "dst": local_config.to_string_lossy()
    }))
}

/// Recursively copy all files from `src` to `dst`, creating dirs as needed.
/// Returns the total number of files copied.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<usize> {
    let mut count = 0;
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            count += copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
            count += 1;
        }
    }
    Ok(count)
}

// ── #188 run_regression_suite ─────────────────────────────────────────────────

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_run_regression_suite(args: Value) -> Result<Value, String> {
    let tag_filter = args.get("tag").and_then(Value::as_str).unwrap_or("").to_string();
    let suite_timeout = args.get("timeout_secs").and_then(Value::as_u64).unwrap_or(3600);

    // Discover all regression tests (same logic as list_tests but filtered to agora_regression/)
    let all_tests = crate::test_runner::list_tests();
    let suite: Vec<_> = all_tests.into_iter().filter(|t| {
        // Only regression tests
        let is_regression = t.id.starts_with("agora_") &&
            crate::platform::config_path("tests/agora_regression")
                .join(format!("{}.yaml", t.id)).exists();
        // Tag filter
        let tag_ok = tag_filter.is_empty() || t.tags.iter().any(|tg| tg == &tag_filter);
        is_regression && tag_ok
    }).collect();

    let total = suite.len();
    if total == 0 {
        return Ok(json!({ "total": 0, "passed": 0, "failed": 0, "timeout": 0,
            "results": [], "summary": "No matching tests found" }));
    }

    // Launch all tests sequentially (TEST_RUN_LOCK enforces serial execution)
    let mut run_ids: Vec<(String, String)> = Vec::new(); // (test_id, run_id)
    for t in &suite {
        match crate::test_runner::spawn_run_async(t.id.clone(), false) {
            Ok(run_id) => run_ids.push((t.id.clone(), run_id)),
            Err(e) => tracing::warn!("[run_regression_suite] failed to queue '{}': {e}", t.id),
        }
    }

    // Poll until all complete or overall timeout
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(suite_timeout);
    let mut results: Vec<Value> = Vec::new();
    let mut pending: std::collections::HashSet<String> =
        run_ids.iter().map(|(_, rid)| rid.clone()).collect();
    let mut done_map: std::collections::HashMap<String, Value> = std::collections::HashMap::new();

    while !pending.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let mut newly_done = Vec::new();
        for run_id in &pending {
            if let Some(state) = crate::test_runner::runs::get(run_id) {
                let is_terminal = matches!(state.phase,
                    crate::test_runner::runs::RunPhase::Complete(_) |
                    crate::test_runner::runs::RunPhase::Error(_));
                if is_terminal {
                    newly_done.push(run_id.clone());
                    done_map.insert(run_id.clone(), crate::test_runner::runs::to_json(&state));
                }
            }
        }
        for rid in newly_done { pending.remove(&rid); }
    }

    // Timed-out runs
    for run_id in &pending {
        let _ = crate::test_runner::runs::kill_run(run_id);
        if let Some(state) = crate::test_runner::runs::get(run_id) {
            done_map.insert(run_id.clone(), crate::test_runner::runs::to_json(&state));
        }
    }

    // Build results list — include console stats from SQLite (#223)
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut timed_out = 0usize;
    let mut console_errors_total = 0u64;
    let mut console_warnings_total = 0u64;
    for (test_id, run_id) in &run_ids {
        let result = done_map.get(run_id).cloned().unwrap_or(json!({ "status": "unknown" }));
        let status = result["status"].as_str().unwrap_or("unknown");
        match status {
            "passed" => passed += 1,
            "timeout" => timed_out += 1,
            _ => failed += 1,
        }
        // Pull console stats from SQLite (written at test completion).
        let (ce, cw) = crate::test_runner::store::get_console_log(run_id)
            .as_deref()
            .map(parse_console_counts)
            .unwrap_or((0, 0));
        console_errors_total  += ce as u64;
        console_warnings_total += cw as u64;
        let flag = if ce > 0 { "error" } else if cw > 0 { "warning" } else { "ok" };
        results.push(json!({
            "test_id":          test_id,
            "run_id":           run_id,
            "status":           status,
            "duration_ms":      result["details"]["duration_ms"],
            "error":            result["details"]["error"],
            "replay_mode":      result.get("replay_mode"),
            "console_errors":   ce,
            "console_warnings": cw,
            "console_flag":     flag,
        }));
    }

    // Sort: failures first, then console errors
    results.sort_by(|a, b| {
        let a_bad = (a["status"] != "passed") as u8;
        let b_bad = (b["status"] != "passed") as u8;
        b_bad.cmp(&a_bad)
            .then(b["console_errors"].as_u64().cmp(&a["console_errors"].as_u64()))
    });

    let summary = format!("{passed}/{total} PASS — failed: {failed}, timeout: {timed_out}");
    let recommendation = if failed > 0 || timed_out > 0 {
        format!("{} tests failed/timed out — check 'status' and 'error' fields", failed + timed_out)
    } else if console_errors_total > 0 {
        format!("All passed but {} console errors detected — review with get_test_result", console_errors_total)
    } else {
        format!("All {total} tests passed with clean console ✓")
    };
    tracing::info!("[run_regression_suite] {}", summary);

    Ok(json!({
        "total":                  total,
        "passed":                 passed,
        "failed":                 failed,
        "timeout":                timed_out,
        "console_errors_total":   console_errors_total,
        "console_warnings_total": console_warnings_total,
        "recommendation":         recommendation,
        "results":                results,
        "summary":                summary,
    }))
}

// ── #193 sirin_preflight ──────────────────────────────────────────────────────

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_sirin_preflight() -> Result<Value, String> {
    let mut warnings: Vec<String> = Vec::new();
    let mut ready = true;

    // 1. Sirin MCP self-check (always true if we got here)
    let sirin_ok = true;

    // 2. Config sync: compare file counts repo vs LOCALAPPDATA
    let repo_dir = std::env::current_dir().ok().map(|d| d.join("config/tests"));
    let local_dir = Some(crate::platform::app_data_dir().join("config/tests"));

    let (repo_count, local_count) = match (repo_dir.as_ref(), local_dir.as_ref()) {
        (Some(r), Some(l)) => {
            let rc = count_yaml_files(r);
            let lc = count_yaml_files(l);
            (rc, lc)
        }
        _ => (0usize, 0usize),
    };
    let config_in_sync = repo_count == local_count;
    if !config_in_sync {
        warnings.push(format!(
            "Config out of sync: repo has {} YAMLs, LOCALAPPDATA has {}. Call sync_config.",
            repo_count, local_count
        ));
        ready = false;
    }

    // 3. redandan.github.io version check
    let pages_version = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        fetch_github_pages_version()
    ).await.ok().flatten();

    // 4. API health (quick check via agora-ops orient if available, skip if not)
    // We just report what we know from orient tool — skip heavy network calls here

    Ok(json!({
        "ready": ready,
        "sirin_mcp": { "ok": sirin_ok, "port": 7700 },
        "config_sync": {
            "in_sync": config_in_sync,
            "repo_yaml_count": repo_count,
            "localappdata_yaml_count": local_count
        },
        "github_pages": {
            "url": "https://redandan.github.io",
            "version": pages_version.as_deref().unwrap_or("unknown"),
            "reachable": pages_version.is_some()
        },
        "warnings": warnings
    }))
}

fn count_yaml_files(dir: &std::path::Path) -> usize {
    if !dir.exists() { return 0; }
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() { count += count_yaml_files(&p); }
            else if p.extension().map(|e| e == "yaml").unwrap_or(false) { count += 1; }
        }
    }
    count
}

async fn fetch_github_pages_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build().ok()?;
    let html = client.get("https://redandan.github.io/")
        .header("User-Agent", "Sirin-preflight/1.0")
        .send().await.ok()?
        .text().await.ok()?;
    // Extract <meta name="version" content="X.Y.Z">
    html.split("name=\"version\"").nth(1)
        .and_then(|s| s.split("content=\"").nth(1))
        .and_then(|s| s.split('"').next())
        .map(|s| s.to_string())
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_page_state(args: Value) -> Result<Value, String> {
    let include_screenshot = args.get("include_screenshot").and_then(Value::as_bool).unwrap_or(true);
    let include_ax         = args.get("include_ax").and_then(Value::as_bool).unwrap_or(true);
    let max_ax_nodes       = args.get("max_ax_nodes").and_then(Value::as_u64).unwrap_or(50) as usize;

    blocking("page_state", move || -> Result<Value, String> {
        use crate::browser;

        // Basic state — always collected.
        let url   = browser::current_url().unwrap_or_default();
        let title = browser::page_title().unwrap_or_default();

        // Console messages (last 20 entries).
        let console_raw = browser::console_messages(20).unwrap_or_else(|_| "[]".into());
        let console_val: Value = serde_json::from_str(&console_raw).unwrap_or(json!([]));

        // Recent network requests (last 20 entries).
        let network_raw = browser::captured_requests(20).unwrap_or_else(|_| "[]".into());
        let network_val: Value = serde_json::from_str(&network_raw).unwrap_or(json!([]));

        let mut result = json!({
            "url":     url,
            "title":   title,
            "console": console_val,
            "network": network_val,
        });

        // Accessibility tree — slim text summary.
        if include_ax {
            match crate::browser_ax::get_full_tree(false) {
                Ok(nodes) => {
                    let limited: Vec<_> = nodes.into_iter().take(max_ax_nodes).collect();
                    let text = limited.iter()
                        .map(|n| format!(
                            "[{}] {} \"{}\"",
                            n.role.as_deref().unwrap_or("?"),
                            n.backend_id.map(|id| id.to_string()).unwrap_or_else(|| "-".into()),
                            n.name.as_deref().unwrap_or(""),
                        ))
                        .collect::<Vec<_>>()
                        .join("\n");
                    result["ax_tree_text"]  = json!(text);
                    result["ax_node_count"] = json!(limited.len());
                }
                Err(e) => { result["ax_error"] = json!(e); }
            }
        }

        // Screenshot — JPEG 80% quality, Base64 encoded.
        if include_screenshot {
            match browser::screenshot_jpeg(80) {
                Ok(jpeg) => {
                    result["screenshot_jpeg_b64"]   = json!(base64_encode(&jpeg));
                    result["screenshot_size_bytes"] = json!(jpeg.len());
                }
                Err(e) => { result["screenshot_error"] = json!(e); }
            }
        }

        Ok(result)
    })
    .await
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_get_full_observation(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?;
    let step   = args["step"].as_u64().ok_or("Missing step (non-negative integer)")? as usize;
    match crate::test_runner::runs::get_full_observation(run_id, step) {
        Some(content) => Ok(json!({
            "run_id": run_id,
            "step": step,
            "content": content,
            "char_count": content.chars().count(),
        })),
        None => Err(format!("observation for run_id '{run_id}' step {step} not found")),
    }
}

/// Issue #39: per-run trace log.  Reads the persisted `history_json` blob
/// for `run_id` and projects each step into a trace event with the new
/// metadata fields (llm_model, llm_latency_ms, kb_hits, parse_errors, ts).
// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) fn call_get_run_trace(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?;
    let (test_id, started_at, status, history_json) =
        crate::test_runner::store::find_history_by_run_id(run_id)
            .ok_or_else(|| format!("run_id '{run_id}' not found in test_runs"))?;
    let raw = history_json.unwrap_or_else(|| "[]".into());
    let history: Vec<crate::test_runner::executor::TestStep> =
        serde_json::from_str(&raw).map_err(|e| format!("history_json parse: {e}"))?;

    let mut total_latency: u64 = 0;
    let mut latency_samples: u64 = 0;
    let mut all_kb_hits: std::collections::BTreeSet<String> = Default::default();
    let steps: Vec<Value> = history.iter().enumerate().map(|(i, s)| {
        if let Some(ms) = s.llm_latency_ms { total_latency += ms; latency_samples += 1; }
        for k in &s.kb_hits { all_kb_hits.insert(k.clone()); }
        let action_short = s.action.get("action").cloned()
            .unwrap_or_else(|| s.action.clone());
        json!({
            "step": i,
            "ts": s.ts,
            "llm_model": s.llm_model,
            "llm_latency_ms": s.llm_latency_ms,
            "llm_tokens": s.llm_tokens,
            "kb_hits": s.kb_hits,
            "parse_errors": s.parse_errors,
            "action": action_short,
            "obs_chars": s.observation.chars().count(),
        })
    }).collect();

    let avg = if latency_samples > 0 { total_latency / latency_samples } else { 0 };
    Ok(json!({
        "run_id": run_id,
        "test_id": test_id,
        "started_at": started_at,
        "status": status,
        "summary": {
            "total_steps": history.len(),
            "kb_hits": all_kb_hits.into_iter().collect::<Vec<_>>(),
            "avg_latency_ms": avg,
        },
        "steps": steps,
    }))
}

// ── kb_search / kb_get ────────────────────────────────────────────────────────
//
// Thin MCP wrappers over `kb_client::search` / `kb_client::get`.  Bearer token
// for the upstream KB MCP server lives in `.env` (KB_MCP_BEARER) and never
// crosses into the Chrome extension — Claude in Chrome sees only the result
// text returned by Sirin.

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_kb_search(args: Value) -> Result<Value, String> {
    if !crate::kb_client::enabled() {
        return Ok(json!({ "error": "KB 未啟用，請設定 KB_ENABLED=1" }));
    }
    let query = args["query"]
        .as_str()
        .ok_or("Missing 'query'")?
        .to_string();
    let project = args["project"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(crate::kb_client::default_project);
    let limit = args["limit"].as_u64().unwrap_or(5).min(20) as usize;

    match crate::kb_client::search(&project, &query, limit).await {
        Ok(Some(text)) => Ok(json!({
            "project": project,
            "query":   query,
            "result":  text,
        })),
        Ok(None) => Ok(json!({
            "project": project,
            "query":   query,
            "result":  null,
            "found":   false,
        })),
        Err(e) => Ok(json!({ "error": e })),
    }
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_kb_get(args: Value) -> Result<Value, String> {
    if !crate::kb_client::enabled() {
        return Ok(json!({ "error": "KB 未啟用，請設定 KB_ENABLED=1" }));
    }
    let topic = args["topic_key"]
        .as_str()
        .ok_or("Missing 'topic_key'")?
        .to_string();
    let project = args["project"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(crate::kb_client::default_project);

    match crate::kb_client::get(&project, &topic).await {
        Ok(Some(text)) => Ok(json!({
            "project":   project,
            "topic_key": topic,
            "content":   text,
        })),
        Ok(None) => Ok(json!({
            "project":   project,
            "topic_key": topic,
            "content":   null,
            "found":     false,
        })),
        Err(e) => Ok(json!({ "error": e })),
    }
}

// Migrated to mcp_registry — visibility raised so the registry can invoke it.
pub(crate) async fn call_kb_write(args: Value) -> Result<Value, String> {
    if !crate::kb_client::enabled() {
        return Ok(json!({ "error": "KB 未啟用，請設定 KB_ENABLED=1" }));
    }
    let topic_key = args["topic_key"].as_str().ok_or("Missing 'topic_key'")?.to_string();
    let title     = args["title"].as_str().ok_or("Missing 'title'")?.to_string();
    let content   = args["content"].as_str().ok_or("Missing 'content'")?.to_string();
    let domain    = args["domain"].as_str().ok_or("Missing 'domain'")?.to_string();
    if topic_key.is_empty() || title.is_empty() || content.is_empty() || domain.is_empty() {
        return Err("topic_key, title, content, domain must all be non-empty".to_string());
    }
    let tags      = args["tags"].as_str().unwrap_or("").to_string();
    let file_refs = args["file_refs"].as_str().unwrap_or("").to_string();
    let project   = args["project"].as_str().map(String::from)
        .unwrap_or_else(crate::kb_client::default_project);

    match crate::kb_client::write_raw_to_project(
        &project, &topic_key, &title, &content, &domain, &tags, &file_refs,
    ).await {
        Ok(()) => Ok(json!({
            "project":    project,
            "topic_key":  topic_key,
            "status":     "accepted",
            "layer":      "raw",
            "confidence": 0.6,
        })),
        Err(e) => Ok(json!({ "error": e })),
    }
}

#[cfg(test)]
mod test_runner_mcp_tests {
    use super::*;

    #[test]
    fn list_tests_returns_config_tests() {
        let result = call_list_tests(json!({})).unwrap();
        assert!(result["count"].is_u64());
        // config/tests/wiki_smoke.yaml should be visible
        let tests = result["tests"].as_array().unwrap();
        assert!(tests.iter().any(|t| t["id"] == "wiki_smoke"),
            "wiki_smoke test should be listed: {result:?}");
    }

    #[test]
    fn list_tests_with_tag_filter() {
        let result = call_list_tests(json!({"tag": "smoke"})).unwrap();
        let tests = result["tests"].as_array().unwrap();
        assert!(tests.iter().all(|t| t["tags"].as_array().unwrap()
            .iter().any(|tg| tg == "smoke")));
    }

    #[test]
    fn run_test_async_rejects_missing_test_id() {
        let result = call_run_test_async(json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn run_test_async_rejects_unknown_test() {
        let result = call_run_test_async(json!({"test_id": "nonexistent_test_xyz"}));
        assert!(result.is_err(), "should reject unknown test_id");
    }

    #[test]
    fn get_test_result_rejects_unknown_run_id() {
        let result = call_get_test_result(json!({"run_id": "run_fake_12345"}));
        assert!(result.is_err());
    }

    #[test]
    fn run_test_batch_rejects_missing_test_ids() {
        assert!(call_run_test_batch(json!({})).is_err());
    }

    #[test]
    fn run_test_batch_rejects_empty_array() {
        let result = call_run_test_batch(json!({"test_ids": []}));
        assert!(result.is_err(), "should reject empty test_ids");
    }

    #[test]
    fn run_test_batch_rejects_unknown_test_id() {
        // The batch should fail-fast if ANY id is unknown.
        let result = call_run_test_batch(json!({
            "test_ids": ["wiki_smoke", "totally_does_not_exist_xyz"],
        }));
        assert!(result.is_err(), "any unknown id must abort the whole batch");
    }

    // Note: a positive-path test that calls call_run_test_batch with a real
    // test_id would spawn a background chrome thread (asynchronous, fully
    // detached from the test).  That risks leaking browser processes and
    // making `cargo test` flaky.  The clamp/dispatch logic is exercised by
    // direct unit tests in `test_runner::mod` instead — see
    // spawn_batch_run_validates_ids and spawn_batch_run_returns_run_ids.

    #[test]
    fn base64_encode_roundtrip() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn run_adhoc_test_requires_url_and_goal() {
        assert!(call_run_adhoc_test(json!({})).is_err());
        assert!(call_run_adhoc_test(json!({"url": "https://x.com"})).is_err());
        assert!(call_run_adhoc_test(json!({"goal": "test something"})).is_err());
    }

    #[test]
    fn list_recent_runs_limits_clamped() {
        // Should not panic with huge limit
        let r = call_list_recent_runs(json!({"limit": 99999})).unwrap();
        assert!(r["count"].is_u64());
        // Default limit when omitted
        let r = call_list_recent_runs(json!({})).unwrap();
        assert!(r["count"].is_u64());
    }

    #[test]
    fn list_fixes_returns_schema() {
        let r = call_list_fixes(json!({})).unwrap();
        assert!(r["count"].is_u64());
        assert!(r["fixes"].is_array());
    }

    #[test]
    fn config_diagnostics_returns_structured_report() {
        let r = call_config_diagnostics().unwrap();
        assert!(r["count"].is_u64());
        assert!(r["issues"].is_array());
        assert!(r["text_report"].is_string());
        // Sum of severities equals count
        let total = r["errors"].as_u64().unwrap()
            + r["warnings"].as_u64().unwrap()
            + r["ok"].as_u64().unwrap()
            + r["issues"].as_array().unwrap().iter()
                .filter(|i| i["severity"] == "info").count() as u64;
        assert_eq!(total, r["count"].as_u64().unwrap());
    }

    #[test]
    fn browser_exec_rejects_missing_action() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(call_browser_exec(json!({}), "test-client"));
        assert!(r.is_err());
    }

    #[test]
    fn resolve_client_id_falls_back_to_user_agent() {
        // A UA that hasn't called `initialize` yet resolves to itself, so
        // audit logs are never empty even for ad-hoc curl probes.
        let id = resolve_client_id("curl/8.7.1");
        assert_eq!(id, "curl/8.7.1");
    }

    #[test]
    fn remember_then_resolve_returns_remembered_client_id() {
        remember_client_id("test-ua-xyz", "my-client@9.9.9");
        assert_eq!(resolve_client_id("test-ua-xyz"), "my-client@9.9.9");
    }

    #[test]
    fn sessions_isolate_clients_by_user_agent() {
        // Race regression guard: two UAs must map to independent client_ids,
        // no matter the order of writes.
        remember_client_id("ua-a", "alice@1.0");
        remember_client_id("ua-b", "bob@2.0");
        remember_client_id("ua-a", "alice@1.0");  // idempotent re-register
        assert_eq!(resolve_client_id("ua-a"), "alice@1.0");
        assert_eq!(resolve_client_id("ua-b"), "bob@2.0");
    }

    #[test]
    fn authz_permissive_config_allows_known_actions() {
        use crate::authz::{AuthzConfig, config::Mode, decide, Decision};
        let cfg = AuthzConfig {
            mode: Mode::Permissive,
            ..AuthzConfig::default()
        };
        // screenshot is in readonly_allow in defaults but here we use a
        // custom permissive config — permissive mode allows everything
        let d = decide("test@1.0", "goto", &json!({"target": "https://example.com/"}), &None, &cfg);
        assert!(matches!(d, Decision::Allow(_)), "permissive should allow goto: {d:?}");

        let d2 = decide("test@1.0", "ax_click", &json!({"backend_id": 1}), &None, &cfg);
        assert!(matches!(d2, Decision::Allow(_)), "permissive should allow ax_click: {d2:?}");
    }

    #[test]
    fn mcp_router_is_constructible() {
        // Smoke test — router should build without panic.  Exercises the
        // TimeoutLayer wiring (layer type inference in particular breaks
        // easily when axum / tower-http versions drift).
        let _: Router = mcp_router();
    }

    #[test]
    fn mcp_request_timeout_is_reasonable() {
        // Must exceed the authz "ask" wait (30 s at L708/734) so that an
        // operator who clicks "Allow" after 25 s still sees the action
        // complete.  Must be short enough to prevent CLOSE_WAIT buildup
        // when a handler stalls on a dead Chrome transport.
        assert!(MCP_REQUEST_TIMEOUT >= Duration::from_secs(60),
            "timeout must outlive authz ask window (30s) + slowest handler");
        assert!(MCP_REQUEST_TIMEOUT <= Duration::from_secs(600),
            "timeout must not exceed 10min (defeats purpose of layer)");
    }

    /// CorsLayer must be constructible whether or not CLAUDE_CHROME_EXT_ID is
    /// set — both branches of `cors_layer()` need to compile and not panic.
    #[test]
    fn cors_layer_strict_mode_constructs() {
        std::env::set_var(
            "CLAUDE_CHROME_EXT_ID",
            "bfnaelmomeimhlpmgjnjophhpkkoljpa",
        );
        let _ = cors_layer();
        std::env::remove_var("CLAUDE_CHROME_EXT_ID");
    }

    #[test]
    fn cors_layer_open_mode_constructs() {
        std::env::remove_var("CLAUDE_CHROME_EXT_ID");
        let _ = cors_layer();
    }

    /// kb_search must short-circuit when KB is disabled (Chrome extensions
    /// that try to call it on a fresh Sirin install must get a clear error
    /// instead of a hang).
    #[tokio::test]
    async fn kb_search_returns_error_when_disabled() {
        std::env::remove_var("KB_ENABLED");
        let r = call_kb_search(json!({ "query": "anything" })).await.unwrap();
        assert!(r.get("error").is_some(), "expected error payload, got {r}");
    }

    #[tokio::test]
    async fn kb_search_rejects_missing_query() {
        std::env::set_var("KB_ENABLED", "1");
        let r = call_kb_search(json!({})).await;
        assert!(r.is_err(), "must reject missing query: {r:?}");
        std::env::remove_var("KB_ENABLED");
    }

    #[tokio::test]
    async fn kb_get_rejects_missing_topic_key() {
        std::env::set_var("KB_ENABLED", "1");
        let r = call_kb_get(json!({})).await;
        assert!(r.is_err(), "must reject missing topic_key: {r:?}");
        std::env::remove_var("KB_ENABLED");
    }

    #[tokio::test]
    async fn kb_get_returns_error_when_disabled() {
        std::env::remove_var("KB_ENABLED");
        let r = call_kb_get(json!({ "topic_key": "any-topic" }))
            .await
            .unwrap();
        assert!(r.get("error").is_some(), "expected error payload, got {r}");
    }

    /// kb_write must appear in tools/list with required schema fields so CiC
    /// can discover + invoke it via `window.sirin.tools.kb_write({...})`.
    #[test]
    fn kb_write_tool_exposed_in_tools_list() {
        let v = handle_tools_list().expect("tools/list ok");
        let tools = v["tools"].as_array().expect("tools array");
        let entry = tools
            .iter()
            .find(|t| t["name"].as_str() == Some("kb_write"))
            .expect("kb_write entry must be present");
        assert!(entry["description"].as_str().unwrap_or("").contains("Knowledge Base"));
        let required = entry["inputSchema"]["required"]
            .as_array()
            .expect("required array");
        let req: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        for k in ["topic_key", "title", "content", "domain"] {
            assert!(req.contains(&k), "required must include {k}, got {req:?}");
        }
        let props = entry["inputSchema"]["properties"]
            .as_object()
            .expect("properties object");
        for k in ["topic_key", "title", "content", "domain", "tags", "file_refs", "project"] {
            assert!(props.contains_key(k), "properties must include {k}");
        }
    }

    #[tokio::test]
    async fn kb_write_returns_error_when_disabled() {
        std::env::remove_var("KB_ENABLED");
        let r = call_kb_write(json!({
            "topic_key": "k", "title": "t", "content": "c", "domain": "d"
        })).await.unwrap();
        assert!(r.get("error").is_some(), "expected error payload, got {r}");
    }

    #[tokio::test]
    async fn kb_write_rejects_missing_required_fields() {
        std::env::set_var("KB_ENABLED", "1");
        let r = call_kb_write(json!({ "title": "t", "content": "c", "domain": "d" })).await;
        assert!(r.is_err(), "must reject missing topic_key: {r:?}");
        let r = call_kb_write(json!({
            "topic_key": "", "title": "t", "content": "c", "domain": "d"
        })).await;
        assert!(r.is_err(), "must reject empty topic_key: {r:?}");
        std::env::remove_var("KB_ENABLED");
    }

    // ── Issue #144: base64_decode must invert base64_encode ─────────────────────

    #[test]
    fn base64_decode_roundtrip_ascii() {
        let src = b"Hello, Sirin!";
        let encoded = base64_encode(src);
        let decoded = base64_decode(&encoded).expect("decode should succeed");
        assert_eq!(decoded, src, "decode(encode(x)) == x for ASCII");
    }

    #[test]
    fn base64_decode_roundtrip_binary() {
        let src: Vec<u8> = (0u8..=255).collect();
        let encoded = base64_encode(&src);
        let decoded = base64_decode(&encoded).expect("decode of all-bytes should succeed");
        assert_eq!(decoded, src, "decode(encode(x)) == x for all byte values");
    }

    #[test]
    fn base64_decode_roundtrip_empty() {
        let encoded = base64_encode(b"");
        let decoded = base64_decode(&encoded).expect("decode empty should succeed");
        assert_eq!(decoded, b"", "empty roundtrip");
    }

    #[test]
    fn base64_decode_rejects_invalid_char() {
        // '@' is not a valid base64 character
        let result = base64_decode("SGVsbG8@");
        assert!(result.is_none(), "should return None for invalid base64 chars");
    }

    // ── #257 Option B end-to-end tests ──────────────────────────────────────
    //
    // The unit tests in `mcp_registry::tests` verify the registry data
    // structure; the parity test in `mcp_parity_tests` verifies source-text
    // consistency.  These tests exercise the actual `handle_tools_list` and
    // `handle_tools_call` entry points so we know the merge + dispatch
    // wiring works end-to-end, not just the data layer.

    #[test]
    fn tools_list_includes_registry_tools() {
        let response = handle_tools_list().expect("handle_tools_list must succeed");
        let tools = response["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter()
            .filter_map(|t| t["name"].as_str())
            .collect();

        // Registry tools must be present (3 currently migrated).
        assert!(names.contains(&"skill_list"),       "skill_list missing: {names:?}");
        assert!(names.contains(&"teams_pending"),    "teams_pending missing: {names:?}");
        assert!(names.contains(&"discovery_status"), "discovery_status missing: {names:?}");

        // Some legacy tools must also still be present (we didn't accidentally
        // delete the literal block).
        assert!(names.contains(&"memory_search"),    "legacy memory_search missing");
        assert!(names.contains(&"run_test_async"),   "legacy run_test_async missing");

        // No tool name is duplicated (registry tools must not also live in
        // the literal block).
        let mut seen = std::collections::HashSet::new();
        for n in &names {
            assert!(seen.insert(n.to_string()), "duplicate tool name: {n}");
        }
    }

    #[tokio::test]
    async fn tools_call_routes_skill_list_through_registry() {
        let envelope = handle_tools_call(
            json!({ "name": "skill_list", "arguments": {} }),
            "test-user-agent",
        ).await.expect("skill_list dispatch must succeed");

        // skill_list is a SyncText handler — envelope shape:
        //   {"content":[{"type":"text","text":"..."}]}
        assert_eq!(envelope["content"][0]["type"], "text",
            "skill_list envelope: {envelope:?}");
        assert!(envelope["content"][0]["text"].is_string());
    }

    #[tokio::test]
    async fn tools_call_routes_discovery_status_through_registry() {
        let envelope = handle_tools_call(
            json!({ "name": "discovery_status", "arguments": {} }),
            "test-user-agent",
        ).await.expect("discovery_status dispatch must succeed");

        // discovery_status is a SyncJson handler — wrap_json renders the
        // structured payload as pretty-printed JSON inside the text field.
        let text = envelope["content"][0]["text"].as_str()
            .expect("envelope text");
        let inner: Value = serde_json::from_str(text)
            .expect("inner payload must be valid JSON");
        // discovery_status returns at minimum a "status" field.
        assert!(inner.get("status").is_some(),
            "discovery_status payload missing status: {inner}");
    }

    #[tokio::test]
    async fn tools_call_legacy_path_still_works_for_unmigrated_tool() {
        // run_test_async is NOT in the registry — must dispatch via the
        // legacy match arm.  Pass an invalid arg so the handler errors
        // cleanly without actually running a test (we just want to verify
        // the dispatch path reaches the handler at all).
        let result = handle_tools_call(
            json!({ "name": "run_test_async", "arguments": {} }),
            "test-user-agent",
        ).await;
        // Either Ok(envelope-with-error-content) or Err — both prove the
        // dispatch path reached the handler.  The key signal we want to
        // rule out is "Unknown tool: run_test_async".
        match result {
            Ok(env) => {
                let text = env["content"][0]["text"].as_str().unwrap_or("");
                assert!(!text.contains("Unknown tool"),
                    "legacy dispatch path broken for run_test_async: {env}");
            }
            Err(e) => {
                assert!(!e.contains("Unknown tool"),
                    "legacy dispatch path broken for run_test_async: {e}");
            }
        }
    }

    #[tokio::test]
    async fn tools_call_returns_unknown_for_truly_unknown_tool() {
        // Sanity: a totally-bogus name still gets the "Unknown tool" error
        // (the fallback isn't accidentally swallowing every unknown).
        let result = handle_tools_call(
            json!({ "name": "this_tool_definitely_does_not_exist_xyz", "arguments": {} }),
            "test-user-agent",
        ).await;
        match result {
            Err(e)  => assert!(e.contains("Unknown tool"), "want Unknown tool error, got: {e}"),
            Ok(env) => panic!("expected error for bogus tool, got envelope: {env}"),
        }
    }

    // ── Phase A diagnostics: tail_app_log + queue_status ──────────────────────

    #[test]
    fn tail_app_log_returns_recent_lines_and_envelope() {
        // Push a marker line through the real log buffer, then assert the
        // handler surfaces it with the envelope fields callers depend on.
        crate::log_buffer::push("[INFO] tail_app_log_test_marker_alpha".to_string());

        let r = call_tail_app_log(json!({"lines": 50})).unwrap();
        assert!(r["lines"].is_array());
        assert!(r["count"].as_u64().unwrap() >= 1);
        assert!(r["total_buffered"].as_u64().unwrap() >= 1);
        assert!(r["version"].as_u64().unwrap() >= 1, "version counter must bump on push");
        assert_eq!(r["filter"]["lines_requested"], 50);
        // Marker visible somewhere in the returned slice.
        let lines = r["lines"].as_array().unwrap();
        assert!(
            lines.iter().any(|l| l.as_str().unwrap_or("").contains("tail_app_log_test_marker_alpha")),
            "marker line must appear in tail output"
        );
    }

    #[test]
    fn tail_app_log_grep_filters_lines() {
        crate::log_buffer::push("[INFO] grep_target_unique_xyz123".to_string());
        crate::log_buffer::push("[INFO] some other unrelated line".to_string());

        let r = call_tail_app_log(json!({"grep": "grep_target_unique_xyz123"})).unwrap();
        let lines = r["lines"].as_array().unwrap();
        assert!(
            lines.iter().all(|l| l.as_str().unwrap_or("").contains("grep_target_unique_xyz123")),
            "all returned lines must match grep filter"
        );
        assert_eq!(r["filter"]["grep"], "grep_target_unique_xyz123");
    }

    #[test]
    fn tail_app_log_clamps_lines_to_300() {
        // > 300 in → clamped to 300 in the filter echo.
        let r = call_tail_app_log(json!({"lines": 9999})).unwrap();
        assert_eq!(r["filter"]["lines_requested"], 300);
        // < 1 → clamped to 1.
        let r = call_tail_app_log(json!({"lines": 0})).unwrap();
        assert_eq!(r["filter"]["lines_requested"], 1);
    }

    #[test]
    fn tail_app_log_level_filter_excludes_other_levels() {
        crate::log_buffer::push(" ERROR [target] level_filter_marker_zzz".to_string());
        crate::log_buffer::push(" INFO [target] level_filter_other_marker".to_string());

        let r = call_tail_app_log(json!({"level": "ERROR", "grep": "level_filter"})).unwrap();
        let lines = r["lines"].as_array().unwrap();
        // Every returned line must contain the ERROR token AND the grep substring.
        for l in lines {
            let s = l.as_str().unwrap_or("");
            assert!(s.contains("ERROR"), "level filter let through non-ERROR: {s}");
            assert!(s.contains("level_filter"));
        }
        assert_eq!(r["filter"]["level"], "ERROR");
    }

    #[test]
    fn queue_status_returns_envelope_with_concurrency_and_drain() {
        let r = call_queue_status(json!({})).unwrap();
        // Concurrency is hardwired to 1 by TEST_RUN_LOCK — pin it so a
        // future change to lift the lock has to update this expectation.
        assert_eq!(r["concurrency"], 1);
        // Required fields whatever the registry state.
        assert!(r["queued_count"].is_u64());
        assert!(r["queued"].is_array());
        assert!(r["oldest_queued_for_secs"].is_i64() || r["oldest_queued_for_secs"].is_u64());
        assert!(r["median_test_secs_last_50"].is_u64());
        assert!(r["p95_test_secs_last_50"].is_u64());
        assert!(r["estimated_drain_secs"].is_u64());
        assert!(r["sampled_recent_runs"].is_u64());
        assert!(r["summary"].is_string());
    }

    #[test]
    fn queue_status_drain_is_count_times_median() {
        // Pin the arithmetic: estimated_drain_secs = queued_count * median.
        let r = call_queue_status(json!({})).unwrap();
        let count = r["queued_count"].as_u64().unwrap();
        let median = r["median_test_secs_last_50"].as_u64().unwrap();
        let drain = r["estimated_drain_secs"].as_u64().unwrap();
        assert_eq!(drain, count * median, "drain estimate must be queued × median");
    }

    // ── Phase B: live_trace + subphase + recent_steps ─────────────────────────

    #[test]
    fn live_trace_rejects_missing_run_id() {
        let r = call_live_trace(json!({}));
        assert!(r.is_err());
    }

    #[test]
    fn live_trace_rejects_unknown_run_id() {
        let r = call_live_trace(json!({"run_id": "run_definitely_does_not_exist_999"}));
        assert!(r.is_err());
    }

    #[test]
    fn live_trace_returns_envelope_with_subphase_for_running_run() {
        // Spin up a fake run via the runs registry, push a step, and call
        // live_trace.  Pins: envelope shape + recent_steps wiring +
        // subphase exposure.
        use crate::test_runner::executor::TestStep;
        use crate::test_runner::runs::{self, RunPhase};

        let run_id = runs::new_run("test_live_trace_envelope");
        runs::set_phase(&run_id, RunPhase::Running {
            step: 7,
            current_action: "shadow_click".to_string(),
        });
        runs::set_subphase(&run_id, "llm_call");

        let step = TestStep {
            thought:            "let me click that button".to_string(),
            action:             json!({"action": "shadow_click", "target": "btn"}),
            observation:        "ok".to_string(),
            llm_model:          Some("gemma-4-e4b-it".to_string()),
            llm_latency_ms:     Some(1234),
            llm_prompt_bytes:   Some(8200),
            llm_response_bytes: Some(420),
            ts:                 Some("2026-05-05T12:00:00+08:00".to_string()),
            ..Default::default()
        };
        runs::push_step_summary(&run_id, step);

        let r = call_live_trace(json!({"run_id": run_id, "last_n": 5})).unwrap();

        // Top-level envelope from runs::to_json + live_trace additions.
        assert_eq!(r["test_id"], "test_live_trace_envelope");
        assert_eq!(r["status"], "running");
        assert_eq!(r["current_subphase"], "llm_call");
        assert!(r["subphase_age_ms"].as_u64().unwrap() < 5000,
            "subphase just set, age must be small");
        assert_eq!(r["recent_steps_count"], 1);
        assert_eq!(r["returned_steps_count"], 1);
        assert_eq!(r["requested_last_n"], 5);

        // Step shape — every Phase-B field surfaced.
        let steps = r["recent_steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        let s0 = &steps[0];
        assert_eq!(s0["iter"], 0);
        assert_eq!(s0["thought"], "let me click that button");
        assert_eq!(s0["action"]["action"], "shadow_click");
        assert_eq!(s0["llm_model"], "gemma-4-e4b-it");
        assert_eq!(s0["llm_latency_ms"], 1234);
        assert_eq!(s0["llm_prompt_bytes"], 8200);
        assert_eq!(s0["llm_response_bytes"], 420);
    }

    #[test]
    fn live_trace_clamps_last_n_and_truncates_huge_action_input() {
        // last_n=999 → clamped to 30; huge string in action input → truncated.
        use crate::test_runner::executor::TestStep;
        use crate::test_runner::runs::{self, RunPhase};

        let run_id = runs::new_run("test_live_trace_clamp");
        runs::set_phase(&run_id, RunPhase::Running { step: 1, current_action: "x".into() });

        let huge = "A".repeat(500);
        runs::push_step_summary(&run_id, TestStep {
            thought: "t".into(),
            action: json!({"action": "screenshot", "data": huge}),
            observation: "ok".into(),
            ..Default::default()
        });

        let r = call_live_trace(json!({"run_id": run_id, "last_n": 999})).unwrap();
        assert_eq!(r["requested_last_n"], 30, "last_n must clamp to 30");

        let s0 = &r["recent_steps"].as_array().unwrap()[0];
        let data = s0["action"]["data"].as_str().unwrap();
        assert!(data.len() < 500, "long action-input string must be truncated");
        assert!(data.contains("truncated"), "preview marker must be present: {data}");
    }

    #[test]
    fn live_trace_recent_steps_buffer_caps_at_30() {
        // Push 35 steps — ring buffer must keep only the most recent 30.
        use crate::test_runner::executor::TestStep;
        use crate::test_runner::runs;

        let run_id = runs::new_run("test_live_trace_cap");
        for i in 0..35 {
            runs::push_step_summary(&run_id, TestStep {
                thought: format!("step-{i}"),
                action: json!({"action": "wait"}),
                observation: "ok".into(),
                ..Default::default()
            });
        }

        let r = call_live_trace(json!({"run_id": run_id, "last_n": 30})).unwrap();
        assert_eq!(r["recent_steps_count"], 30, "buffer caps at 30 (oldest dropped)");
        let steps = r["recent_steps"].as_array().unwrap();
        // Oldest kept should be step-5 (35 - 30 = step-5 onward).
        assert_eq!(steps[0]["thought"], "step-5");
        assert_eq!(steps[29]["thought"], "step-34");
    }

    #[test]
    fn subphase_clears_independent_of_phase() {
        // set_subphase / clear_subphase pair.  After clear, current_subphase
        // becomes null but the parent phase (Running) is unaffected.
        use crate::test_runner::runs::{self, RunPhase};

        let run_id = runs::new_run("test_subphase_clear");
        runs::set_phase(&run_id, RunPhase::Running { step: 1, current_action: "x".into() });
        runs::set_subphase(&run_id, "browser_action");

        let r = call_get_test_result(json!({"run_id": &run_id})).unwrap();
        assert_eq!(r["current_subphase"], "browser_action");

        runs::clear_subphase(&run_id);
        let r = call_get_test_result(json!({"run_id": &run_id})).unwrap();
        assert!(r["current_subphase"].is_null(), "current_subphase must clear to null");
        assert_eq!(r["status"], "running", "parent phase unaffected by subphase clear");
    }
}

/// Minimal base64 decoder (no external dep). Returns None on invalid input.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: [i8; 256] = {
        let mut t = [-1i8; 256];
        let enc = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0usize;
        while i < enc.len() { t[enc[i] as usize] = i as i8; i += 1; }
        t
    };
    let clean: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in &clean {
        let v = TABLE[c as usize];
        if v < 0 { return None; }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 { bits -= 8; out.push((buf >> bits) as u8); }
    }
    Some(out)
}

/// Minimal base64 encoder (no external dep).
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 { out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char); }
        else { out.push('='); }
        if chunk.len() > 2 { out.push(CHARS[(triple & 0x3F) as usize] as char); }
        else { out.push('='); }
    }
    out
}
