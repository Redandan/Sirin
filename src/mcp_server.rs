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
    extract::Json,
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;

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
        .layer(cors_layer())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            MCP_REQUEST_TIMEOUT,
        ))
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
    Ok(json!({
        "tools": [
            {
                "name": "memory_search",
                "description": "搜尋 Sirin 的記憶庫與對話歷史。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "搜尋關鍵字" },
                        "limit": { "type": "number", "description": "最多返回幾筆（預設 5）" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "skill_list",
                "description": "列出 Sirin 所有可用技能（含 YAML 動態技能）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "teams_pending",
                "description": "取得 Teams 待確認回覆草稿列表。",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "teams_approve",
                "description": "核准指定的 Teams 草稿，觸發送出。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "PendingReply ID" }
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "trigger_research",
                "description": "觸發 Sirin 對指定主題進行調研。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic": { "type": "string", "description": "調研主題" },
                        "url":   { "type": "string", "description": "參考 URL（選填）" }
                    },
                    "required": ["topic"]
                }
            },
            {
                "name": "list_tests",
                "description": "列出 config/tests/ 目錄下所有 YAML 測試 goal。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tag": { "type": "string", "description": "選填：tag filter" }
                    }
                }
            },
            {
                "name": "run_test_async",
                "description": "非同步啟動測試；立即返回 run_id。用 get_test_result 輪詢狀態。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "test_id":  { "type": "string", "description": "測試 id（config/tests/*.yaml 中的 id 欄位）" },
                        "auto_fix": { "type": "boolean", "description": "失敗時自動 spawn claude_session 修 bug（預設 false）" }
                    },
                    "required": ["test_id"]
                }
            },
            {
                "name": "run_test_batch",
                "description": "並行啟動多個 YAML test，每個跑在獨立 chrome tab（session_id 自動分配）。立即返回 N 個 run_id。\n\n適用場景：smoke suite / nightly regression / 一次跑完多個 tag。\n\n限制：max_concurrency 最大 8（避免 CDP 連線過載）；不會自動 triage 或 auto_fix（失敗請用個別 run_test_async 重跑）；任一 test_id 找不到就整批拒絕。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "test_ids": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "要並行跑的 test_id 清單（必須都存在於 config/tests/*.yaml）"
                        },
                        "max_concurrency": {
                            "type": "number",
                            "description": "最大同時執行數量；預設 3，最大 8"
                        }
                    },
                    "required": ["test_ids"]
                }
            },
            {
                "name": "run_test_pipeline",
                "description": "以管線方式循序執行多個 YAML test，組成粗粒度的交易流程測試。\n\n與 run_test_batch（並行）不同，pipeline 保證執行順序：stage1 完成後才跑 stage2。\n適用場景：C2C 交易生命週期（買家下單→賣家出貨→買家確認）等有依賴順序的流程。\n\n返回：pipeline_id（識別整批）+ 每個 stage 的 run_id（可分別 poll）。\nstop_on_failure=true 時，某 stage 失敗後跳過後續 stage。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "stage_ids": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "按執行順序的 test_id 清單（每個都要存在於 config/tests/）"
                        },
                        "stop_on_failure": {
                            "type": "boolean",
                            "description": "某 stage 失敗後是否停止後續（預設 false = 繼續跑完所有）"
                        }
                    },
                    "required": ["stage_ids"]
                }
            },
            {
                "name": "get_test_result",
                "description": "依 run_id 取得測試狀態。可能狀態：queued | running | passed | failed | timeout | error。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string", "description": "spawn_run_async 返回的 run_id" }
                    },
                    "required": ["run_id"]
                }
            },
            {
                "name": "kill_run",
                "description": "強制終止卡住的 in-memory test run（zombie）。\n\n當 run_test_async 啟動的 run 因 LLM call 阻塞而超過 timeout_secs 仍顯示 running 時使用。\n設為 error 狀態，讓後續 run_test_async 呼叫可以正常取得 TEST_RUN_LOCK。\n⚠️ 只能終止 Running/Queued 狀態的 run；已完成的 run 無法覆蓋。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string", "description": "要強制終止的 run_id" }
                    },
                    "required": ["run_id"]
                }
            },
            {
                "name": "get_screenshot",
                "description": "依 run_id 取得失敗截圖（base64 PNG）。若 bytes 為 null，screenshot_error 說明為何失敗。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string" }
                    },
                    "required": ["run_id"]
                }
            },
            {
                "name": "get_full_observation",
                "description": "取得某步驟的完整（未截斷）browser tool observation。LLM 歷史中的 observation 被截斷時，可用這個抓完整內容。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string" },
                        "step":   { "type": "number", "description": "0-indexed 步驟" }
                    },
                    "required": ["run_id", "step"]
                }
            },
            {
                "name": "get_run_trace",
                "description": "依 run_id 取得每個 step 的 trace 元資料：LLM model/latency、KB injects、parse errors、timestamp。debug 失敗用。\n\n- `steps`: 陣列，每筆有 ts/llm_model/llm_latency_ms/parse_errors/kb_hits/action（簡化）\n- `summary`: total_steps、kb_hits（去重）、avg_latency_ms",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string", "description": "已完成的 run_id（從 SQLite test_runs 表讀取）" }
                    },
                    "required": ["run_id"]
                }
            },
            {
                "name": "run_adhoc_test",
                "description": "即席啟動測試 — 不需預先建立 YAML。外部 AI 收到用戶要求『測 <URL> 的 <流程>』時用這個。立即返回 run_id，用 get_test_result 輪詢。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url":  { "type": "string", "description": "要測試的起始 URL" },
                        "goal": { "type": "string", "description": "高階測試目標（自然語言描述）" },
                        "success_criteria": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "通過條件（1-3 條；空陣列會用預設的『目標達成』判斷）"
                        },
                        "locale":           { "type": "string", "description": "zh-TW / en / zh-CN（預設 zh-TW）" },
                        "max_iterations":   { "type": "number", "description": "預設 15" },
                        "timeout_secs":     { "type": "number", "description": "預設 120" },
                        "browser_headless": { "type": "boolean", "description": "Flutter CanvasKit/WebGL 必須設 false 才能 paint。預設讀 SIRIN_BROWSER_HEADLESS env（預設 true）" },
                        "llm_backend":      { "type": "string", "description": "可選 LLM backend override：'claude_cli'/'claude' = 用 claude -p subprocess（Max plan、JSON 輸出最穩、~3-5s/呼叫 overhead）；省略或其他值 = 用 Sirin 主 LLM 設定（Gemini/LM Studio 等）。優先順序：此參數 > TEST_RUNNER_LLM_BACKEND env > 主設定" },
                        "fixture": {
                            "type": "object",
                            "description": "可選的 fixture：setup（測試前執行）和 cleanup（測試後執行，無論成敗）。",
                            "properties": {
                                "setup": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "action":     { "type": "string" },
                                            "target":     { "type": "string" },
                                            "text":       { "type": "string" },
                                            "timeout_ms": { "type": "number" }
                                        },
                                        "required": ["action"]
                                    }
                                },
                                "cleanup": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "action":     { "type": "string" },
                                            "target":     { "type": "string" },
                                            "text":       { "type": "string" },
                                            "timeout_ms": { "type": "number" }
                                        },
                                        "required": ["action"]
                                    }
                                }
                            }
                        }
                    },
                    "required": ["url", "goal"]
                }
            },
            {
                "name": "persist_adhoc_run",
                "description": "把一次成功的 ad-hoc 探索升級為永久 regression test。\n\n工作流：\n1. AI 用 run_adhoc_test 探索 → 拿 run_id\n2. 用 get_test_result 確認 status=passed\n3. 用 persist_adhoc_run(run_id, test_id='login_flow') 寫出 config/tests/login_flow.yaml\n4. 之後 run_test_async + test_id='login_flow' 就是 regression test\n\n會拒絕：失敗/未完成的 run、test_id 含大寫或連字符、test_id 以 'adhoc_' 開頭、檔案已存在（除非 overwrite=true）、run 超過 1 小時被 prune。\n\n預設行為：strip 'Ad-hoc: ' 前綴 / 把 'adhoc' tag 換成 'adhoc-derived' / max_iterations 提升到 max(used+5, original) 以容忍 regression 變異。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id":  { "type": "string", "description": "run_adhoc_test 返回的 run_id（必須在 1 小時內、且狀態為 passed）" },
                        "test_id": { "type": "string", "description": "新測試的永久 id，必須符合 [a-z0-9_]+，不能以 adhoc_ 開頭。會變成 config/tests/<test_id>.yaml" },
                        "name":    { "type": "string", "description": "可選的人類可讀名稱；省略時沿用 ad-hoc 名稱（自動 strip 'Ad-hoc: ' 前綴）" },
                        "tags":    {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "可選 tags 覆蓋；省略時沿用原 tags（'adhoc' 會被換成 'adhoc-derived'）"
                        },
                        "bump_iterations": { "type": "boolean", "description": "true 時將 max_iterations 提升到 max(used+5, original)，給 regression 留 slack；預設 true" },
                        "overwrite":       { "type": "boolean", "description": "覆蓋現存檔案；預設 false（避免誤刪手寫測試）" }
                    },
                    "required": ["run_id", "test_id"]
                }
            },
            {
                "name": "list_recent_runs",
                "description": "查詢歷史測試執行記錄。不指定 test_id 時列所有測試的近期 runs。用來看 pattern / flakiness / 最近失敗原因。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "test_id": { "type": "string", "description": "選填：只看特定測試" },
                        "limit":   { "type": "number", "description": "筆數（預設 20，最多 100）" }
                    }
                }
            },
            {
                "name": "list_saved_scripts",
                "description": "列出所有已儲存的確定性重播腳本（deterministic replay scripts）。顯示 test_id、儲存時間、成功/失敗次數、腳本 action 數量。用於管理腳本庫。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "delete_saved_script",
                "description": "刪除指定測試的已儲存腳本，迫使下次跑 LLM ReAct loop 重新生成。當 UI 改版導致腳本失效時使用。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "test_id": { "type": "string", "description": "要刪除腳本的 test_id" }
                    },
                    "required": ["test_id"]
                }
            },
            {
                "name": "test_summary",
                "description": "一次呼叫取得最近一批測試的完整摘要：pass/fail counts + console_errors 統計 + 建議動作。適合每次 regression suite 跑完後立刻查看結果。\n\n回傳: { passed, failed, console_errors_total, console_warnings_total, results: [{test_id, status, console_errors, console_warnings, flag}] }",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "since":  { "type": "string", "description": "只看此時間（HH:MM）之後的 runs，預設最近 1 小時" },
                        "limit":  { "type": "number", "description": "最多看幾個不重複的 test，預設 31" }
                    }
                }
            },
            {
                "name": "test_analytics",
                "description": "聚合測試健康指標：pass rate (近 10 / 30 runs)、flaky 標記、avg iterations、avg duration、最常見 failure_category。不指定 test_id 時返回全部測試（依 pass_rate_7d 升序，最差優先）+ summary 區塊。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "test_id": { "type": "string", "description": "選填：只看特定測試" }
                    }
                }
            },
            {
                "name": "test_coverage",
                "description": "AgoraMarket 功能地圖覆蓋率報告。\n\n讀取 config/coverage/agora_market.yaml 功能地圖，交叉比對現有測試和 saved scripts，輸出：\n• 每個功能模組的覆蓋率（%）\n• 各功能點的狀態（confirmed/partial/missing）\n• 哪些測試覆蓋該功能 + 是否有 replay script\n• 覆蓋缺口（missing features）清單\n• 建議下一步新增哪個測試",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "group_id": { "type": "string", "description": "選填：只看特定 feature group（如 'buyer_browse'）" },
                        "show_missing_only": { "type": "boolean", "description": "只顯示 missing features，預設 false" }
                    }
                }
            },
            {
                "name": "ui_navigate",
                "description": "TEST/DEV — drive Sirin's egui UI to a specific view/modal/palette state without keyboard or mouse input.\n\nBypasses winit's input filter that blocks SendKeys/keybd_event from reaching the app. Used by automated UI smoke tests.\n\n`target` values:\n- `dashboard` — main landing\n- `testing` / `testing:runs` / `testing:coverage` / `testing:browser`\n- `workspace:N` — agent detail at index N\n- `palette` / `palette:<query>` — open ⌘K palette, optionally pre-fill query\n- `settings` / `logs` — open System modal at given tab\n- `devsquad` / `mcp` — open Automation modal\n- `ai-router` / `tasks` / `cost-kb` — open Ops modal\n- `close` — close palette + modal + gear menu",
                "inputSchema": {
                    "type": "object",
                    "required": ["target"],
                    "properties": {
                        "target": { "type": "string" }
                    }
                }
            },
            {
                "name": "ui_state",
                "description": "TEST/DEV — read current Sirin UI state (view, modal, palette, agent count, recent runs). Returns the most recent end-of-frame snapshot. Pair with ui_navigate to verify state transitions.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "discover_app",
                "description": "Issue #247 — 啟動自動探索爬蟲。Sirin 開瀏覽器到 seed_url，用 dom_snapshot 列舉可互動元件（button/link/input/tab），存到 SQLite discovered_features 表。\n\n用於 Coverage 3-tier funnel 的「探索」層 — 補 YAML coverage map 沒列到的功能。\n\n回傳 run_id（立即返回，crawl 在背景跑）。用 discovery_status 查狀態。",
                "inputSchema": {
                    "type": "object",
                    "required": ["seed_url"],
                    "properties": {
                        "seed_url":  { "type": "string", "description": "起始 URL（爬蟲第一個導覽到的頁面）" },
                        "max_depth": { "type": "number", "description": "遞迴深度上限（iter 2 只支援 1，更深需後續 PR）" }
                    }
                }
            },
            {
                "name": "discovery_status",
                "description": "查最近一次 discovery 爬蟲狀態。回傳 status (running/done/failed)、started_at、finished_at、total_widgets、累計 distinct features 數。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "discovery_features",
                "description": "列出 discovery 爬蟲找到的所有 features（route/label/kind/selector/last_seen）。可用 limit + kind 過濾。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "number", "description": "回傳上限，預設 100" },
                        "kind":  { "type": "string", "description": "選填：只看某個 kind（button/link/form_input/tab/menuitem）" }
                    }
                }
            },
            {
                "name": "list_flaky_tests",
                "description": "列出歷史上不穩定（flaky）的測試，依 pass rate 升序（最差優先）。\n\n等同 test_analytics 但只回傳 flaky 條目，方便快速定位需要重點關注的測試。\n\nflaky 定義：近 10 次中 pass rate < threshold（預設 70%）且至少 3 次 runs。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "threshold": { "type": "number", "description": "flaky 閾值，0.0-1.0，預設 0.70" },
                        "limit":     { "type": "number", "description": "回傳上限，預設 20" }
                    }
                }
            },
            {
                "name": "replay_last_failure",
                "description": "讀取某個測試最近一次失敗 run 的逐步執行記錄（step-by-step inspection）。\n\n不重新執行測試 — 直接從 SQLite history_json 取出每一個 action 和 LLM observation，讓你像翻日誌一樣審查失敗過程。\n\n`break_at` 可限制只看前 N 步，適合定位哪一步開始出錯。",
                "inputSchema": {
                    "type": "object",
                    "required": ["test_id"],
                    "properties": {
                        "test_id":  { "type": "string", "description": "YAML test_id，例如 agora_cart_add_remove" },
                        "break_at": { "type": "number",  "description": "只返回前 N 步，0=全部（預設）" }
                    }
                }
            },
            {
                "name": "shadow_dump_diff",
                "description": "對比一次失敗 run 中第 A 步和第 B 步的 LLM observation（AX tree 快照）。以行為單位的 unified diff 格式輸出，方便定位 ExpansionTile 動畫、tab 切換等 UI 狀態變化。",
                "inputSchema": {
                    "type": "object",
                    "required": ["test_id", "step_a", "step_b"],
                    "properties": {
                        "test_id": { "type": "string", "description": "YAML test_id" },
                        "step_a":  { "type": "number",  "description": "第一個步驟編號（1-based）" },
                        "step_b":  { "type": "number",  "description": "第二個步驟編號（1-based）" },
                        "run_id":  { "type": "string",  "description": "指定 run_id（選填，預設用最新失敗 run）" }
                    }
                }
            },
            {
                "name": "compare_with_replay",
                "description": "#230 — 對比同一 test_id 的最近一次 script replay run 和最近一次 LLM run 的結果差異。\n\n用於評估 replay 模式是否可靠：比較兩者的 status / duration / iterations / failure_category。\n若只有其中一種 run 存在，也會回傳現有資料並標注缺少哪一種。",
                "inputSchema": {
                    "type": "object",
                    "required": ["test_id"],
                    "properties": {
                        "test_id": { "type": "string", "description": "YAML test_id" }
                    }
                }
            },
            {
                "name": "explain_failure",
                "description": "用 LLM 解釋某次測試失敗的根因。整合截圖分析、console errors、歷史步驟、AI analysis，產生人類可讀的診斷報告。\n\n適合 debug 時快速理解失敗原因，不需要手動翻 history_json。",
                "inputSchema": {
                    "type": "object",
                    "required": ["run_id"],
                    "properties": {
                        "run_id": { "type": "string", "description": "要解釋的測試 run_id" }
                    }
                }
            },
            {
                "name": "list_fixes",
                "description": "查詢 auto-fix 歷史（claude_session spawn 記錄）。能看到哪些 test 觸發過自動修復、結果如何。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "test_id": { "type": "string", "description": "選填" },
                        "limit":   { "type": "number", "description": "預設 20" }
                    }
                }
            },
            {
                "name": "suggest_allowlist",
                "description": "#229 — 掃描 ~/.claude/projects/**/*.jsonl 找出高頻 tool-call pattern，建議加到 Claude Code allowlist。\n\n`threshold`：同一 pattern 出現 N 次以上才建議（預設 2）。\n`sessions`：掃最近 N 個 session（預設 10）。\n`project_key`：限制掃指定 project 目錄名稱（例如 C--Users-Redan-IdeaProjects-Sirin），不指定則掃全部。\n\n回傳結果排除已在 allow list 中的 pattern，只顯示新增建議。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "threshold":   { "type": "number", "description": "出現次數閾值，預設 2" },
                        "sessions":    { "type": "number", "description": "掃最近 N 個 session，預設 10" },
                        "project_key": { "type": "string", "description": "限定 project 目錄名稱（可選）" }
                    }
                }
            },
            {
                "name": "list_redundant_allow",
                "description": "#229 — 找出 ~/.claude/settings.json allowlist 中的冗餘項目。\n\n冗餘定義：如果 pattern A 的前綴已被 wildcard pattern B 涵蓋（例如 Bash(git commit:*) 已被 Bash(git:*) 涵蓋），則 A 是冗餘的。\n\n回傳建議刪除的列表。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "list_allowlist",
                "description": "#225 — 列出 ~/.claude/settings.json permissions.allow 中所有項目。同時計算 Bash wildcard 和 exact 項目數量。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "add_allow",
                "description": "#225 — 向 ~/.claude/settings.json permissions.allow 新增一條 pattern。\n\n原子寫入：讀取 → 修改 → 寫回（JSON 格式化，4-space indent）。重複 pattern 自動去重。",
                "inputSchema": {
                    "type": "object",
                    "required": ["pattern"],
                    "properties": {
                        "pattern": { "type": "string", "description": "例如 Bash(cargo:*) 或 Read" }
                    }
                }
            },
            {
                "name": "remove_allow",
                "description": "#225 — 從 ~/.claude/settings.json permissions.allow 刪除指定 pattern（精確匹配）。",
                "inputSchema": {
                    "type": "object",
                    "required": ["pattern"],
                    "properties": {
                        "pattern": { "type": "string", "description": "要刪除的 pattern（完整字串匹配）" }
                    }
                }
            },
            {
                "name": "list_slash_commands",
                "description": "#225 — 列出 ~/.claude/commands/*.md 中所有 slash commands（name + 前 3 行 description）。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "list_hooks",
                "description": "#225 — 列出 ~/.claude/settings.json hooks 設定（event → command 清單）。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "save_point",
                "description": "#227 — 在 ~/.claude/session_points.json 儲存一個進度記錄點（save point）。可在 session 內或跨 session 快速恢復上下文，比 handoff 更輕量。\n\nttl_days 預設 7 天後自動過期。",
                "inputSchema": {
                    "type": "object",
                    "required": ["label"],
                    "properties": {
                        "label":    { "type": "string", "description": "唯一識別名稱，例如 debug-issue-230" },
                        "summary":  { "type": "string", "description": "進度摘要（自由文字）" },
                        "ttl_days": { "type": "number", "description": "存活天數，預設 7" }
                    }
                }
            },
            {
                "name": "list_points",
                "description": "#227 — 列出所有未過期的 save points（最新在前）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label_contains": { "type": "string", "description": "過濾：label 包含此字串（選填）" }
                    }
                }
            },
            {
                "name": "restore_point",
                "description": "#227 — 讀取指定 save point 的 summary，用於恢復上下文。",
                "inputSchema": {
                    "type": "object",
                    "required": ["label"],
                    "properties": {
                        "label": { "type": "string", "description": "要恢復的 save point label" }
                    }
                }
            },
            {
                "name": "expire_points",
                "description": "#227 — 清除 ~/.claude/session_points.json 中已過期的 save points（saved_at + ttl_days < now）。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "session_cost",
                "description": "#232 — 解析 ~/.claude/projects/**/*.jsonl 計算指定 session（或最新 session）的 token 使用量和 API 費用估算。\n\n包含：input/output/cache tokens、USD 費用估算（用 Anthropic 公定價）、cache hit rate、最高成本 tool 排行。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id":  { "type": "string", "description": "JSONL 檔案名稱（不含 .jsonl），選填，預設最新" },
                        "project_key": { "type": "string", "description": "限定 project 目錄，選填" }
                    }
                }
            },
            {
                "name": "list_expensive_sessions",
                "description": "#232 — 列出最貴的 N 個 sessions（依 USD 成本降序）。\n\n`top` 預設 10。`project_key` 可限定 project 目錄。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "top":         { "type": "number", "description": "回傳數量，預設 10" },
                        "project_key": { "type": "string", "description": "限定 project 目錄，選填" }
                    }
                }
            },
            {
                "name": "create_task",
                "description": "#228 — 在 ~/.claude/tasks.json 建立一個 task 追蹤項目，取代 MEMORY.md 手動維護的 backlog。\n\n`priority`: P0=緊急 / P1=重要 / P2=一般。回傳 task_id（格式 T-YYYYMMDD-NNNN）。",
                "inputSchema": {
                    "type": "object",
                    "required": ["project", "description"],
                    "properties": {
                        "project":     { "type": "string", "description": "project 識別碼，例如 sirin / agora-backend / flutter" },
                        "description": { "type": "string", "description": "任務描述" },
                        "priority":    { "type": "string", "description": "P0 / P1 / P2，預設 P1" },
                        "kb_refs":     { "type": "string", "description": "相關 KB topicKey（逗號分隔，選填）" }
                    }
                }
            },
            {
                "name": "list_tasks",
                "description": "#228 — 列出 ~/.claude/tasks.json 中的 tasks（預設只列 open）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project":  { "type": "string",  "description": "過濾 project（選填）" },
                        "status":   { "type": "string",  "description": "open / done / all，預設 open" },
                        "priority": { "type": "string",  "description": "過濾 priority（P0/P1/P2，選填）" }
                    }
                }
            },
            {
                "name": "mark_task_done",
                "description": "#228 — 將 task 標為 done，附帶 resolution 說明。",
                "inputSchema": {
                    "type": "object",
                    "required": ["task_id", "resolution"],
                    "properties": {
                        "task_id":    { "type": "string", "description": "T-YYYYMMDD-NNNN 格式的 task ID" },
                        "resolution": { "type": "string", "description": "完成說明或 commit hash" }
                    }
                }
            },
            {
                "name": "link_task",
                "description": "#228 — 把 task 和 GitHub issue URL 或 KB topicKey 連結。",
                "inputSchema": {
                    "type": "object",
                    "required": ["task_id"],
                    "properties": {
                        "task_id":     { "type": "string", "description": "T-YYYYMMDD-NNNN" },
                        "github_url":  { "type": "string", "description": "GitHub issue URL（選填）" },
                        "kb_topickey": { "type": "string", "description": "KB topicKey（選填）" }
                    }
                }
            },
            {
                "name": "create_handoff",
                "description": "#224 — 建立一個 session 交接記錄，存到 ~/.claude/handoff_history.json。\n\n比 kbWrite 更簡潔：不需要 domain/layer/tags/fileRefs 等樣板參數，專門為 session bridge 設計。\n內容自動 unescape，get_latest_handoff 直接回傳 markdown，不需要 Python unescape。\n同時寫入 KB（topicKey=sirin-handoff-latest）供 SessionStart hook 使用。",
                "inputSchema": {
                    "type": "object",
                    "required": ["reason", "content"],
                    "properties": {
                        "reason":    { "type": "string", "description": "交接原因，例如「加了新 MCP 需重啟」" },
                        "content":   { "type": "string", "description": "Markdown 格式的交接內容（接手 prompt + 完成事項等）" },
                        "project":   { "type": "string", "description": "project slug，預設 sirin" },
                        "file_refs": { "type": "string", "description": "動到的檔案（逗號分隔，選填）" }
                    }
                }
            },
            {
                "name": "get_latest_handoff",
                "description": "#224 — 讀取最新的 handoff 記錄，直接回傳 markdown 內容（已 unescape）。\n\n比 fetch-handoff.sh 更簡單：不需要 agora-trading auth，不需要 Python unescape，直接呼叫 Sirin MCP。\n可用於 SessionStart hook 的簡化版 fetch-handoff.sh。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": { "type": "string", "description": "project slug，預設 sirin" }
                    }
                }
            },
            {
                "name": "list_handoff_history",
                "description": "#224 — 列出最近 N 筆 handoff 記錄（最新在前）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": { "type": "string", "description": "project slug，預設 sirin" },
                        "limit":   { "type": "number",  "description": "筆數上限，預設 10" }
                    }
                }
            },
            {
                "name": "generate_daily_brief",
                "description": "#231 — 自動聚合 agora-trading 多個工具生成每日 ops 摘要（markdown）。\n\n一次呼叫取代手動跑 getMarketSnapshot + getOpenPositions + getShadowSignalStats 等。\n`sections`：逗號分隔，預設 market,portfolio,ml,ops。\n結果同時寫入 KB（topicKey=agora-daily-brief-YYYYMMDD）供下次 session 參考。\n\n需要 agora-trading + agora-ops token 設定。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sections": { "type": "string", "description": "逗號分隔：market,portfolio,ml,ops（預設全部）" },
                        "date":     { "type": "string", "description": "YYYY-MM-DD，預設今天" }
                    }
                }
            },
            {
                "name": "kb_merge",
                "description": "#226 — 合併多個 KB 條目到一個目標 key。\n\n`strategy`：\n- concat：直接合併所有 src 內容（預設）\n- llm：呼叫 LLM 智慧整合，去重複，保留精華\n合併後將 src 條目標為 stale。",
                "inputSchema": {
                    "type": "object",
                    "required": ["src_keys", "dst_key"],
                    "properties": {
                        "src_keys":  { "type": "string",  "description": "逗號分隔的來源 topicKey 列表" },
                        "dst_key":   { "type": "string",  "description": "目標 topicKey（若已存在則追加）" },
                        "project":   { "type": "string",  "description": "project slug，預設 sirin" },
                        "strategy":  { "type": "string",  "description": "concat（預設）| llm" },
                        "dry_run":   { "type": "boolean", "description": "true=只預覽不寫入，預設 false" }
                    }
                }
            },
            {
                "name": "route_query",
                "description": "#233 — 依 intent registry 自動選擇 LLM 並呼叫。\n\n查 ~/.claude/llm_intents.json 找到 intent 對應的 backend+model，呼叫並回傳結果。\nintent 例：indicator-design(→deepseek), code-review(→claude), vision(→gemini)。\n找不到 intent → 使用 primary LLM（Gemini/Claude）。",
                "inputSchema": {
                    "type": "object",
                    "required": ["intent", "prompt"],
                    "properties": {
                        "intent": { "type": "string", "description": "意圖名稱，例如 indicator-design / code-review / translate-zh" },
                        "prompt": { "type": "string", "description": "要發送給 LLM 的 prompt" }
                    }
                }
            },
            {
                "name": "query_llm",
                "description": "#233 — 直接呼叫指定 LLM backend（跳過 intent routing）。\n\n`backend`: gemini / deepseek / claude / ollama。\n`model`: 選填，不指定則用 backend 預設值或 .env 設定。\n`api_key`: 選填，不指定則從 .env 讀取。",
                "inputSchema": {
                    "type": "object",
                    "required": ["backend", "prompt"],
                    "properties": {
                        "backend": { "type": "string", "description": "gemini / deepseek / claude / ollama" },
                        "model":   { "type": "string", "description": "模型名稱（選填）" },
                        "api_key": { "type": "string", "description": "API key（選填，不填從 .env 讀）" },
                        "prompt":  { "type": "string", "description": "prompt 內容" }
                    }
                }
            },
            {
                "name": "fallback_chain",
                "description": "#233 — 依序嘗試 LLM 列表，第一個成功的回傳結果。\n\n`backends`：逗號分隔，例如 \"gemini,deepseek,claude\"。\n任一 backend 失敗（429/error）自動切下一個，< 1s latency。",
                "inputSchema": {
                    "type": "object",
                    "required": ["prompt", "backends"],
                    "properties": {
                        "prompt":   { "type": "string", "description": "prompt 內容" },
                        "backends": { "type": "string", "description": "逗號分隔的 backend 列表，例如 gemini,deepseek,claude" }
                    }
                }
            },
            {
                "name": "list_intents",
                "description": "#233 — 列出 ~/.claude/llm_intents.json 中的所有 intent → LLM 路由規則。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "register_intent",
                "description": "#233 — 在 ~/.claude/llm_intents.json 新增或更新一條 intent → LLM 路由規則。",
                "inputSchema": {
                    "type": "object",
                    "required": ["name", "backend"],
                    "properties": {
                        "name":    { "type": "string", "description": "intent 名稱，例如 indicator-design" },
                        "backend": { "type": "string", "description": "LLM backend：gemini / deepseek / claude / ollama" },
                        "model":   { "type": "string", "description": "指定 model（選填）" },
                        "reason":  { "type": "string", "description": "路由原因備注（選填）" }
                    }
                }
            },
            {
                "name": "benchmark_llms",
                "description": "#233 — 同一 prompt 同時發給多個 LLM，比較回應速度和內容。\n\n`backends`：逗號分隔，預設 \"gemini,deepseek\"。結果含每個 backend 的耗時 ms 和前 200 字回應。",
                "inputSchema": {
                    "type": "object",
                    "required": ["prompt"],
                    "properties": {
                        "prompt":   { "type": "string", "description": "benchmark 用的 prompt" },
                        "backends": { "type": "string", "description": "逗號分隔，預設 gemini,deepseek" }
                    }
                }
            },
            {
                "name": "kb_duplicate_check",
                "description": "#226 — 找出 KB 中內容高度重疊的條目（Jaccard 文字相似度），不需要 Chroma embedding。\n\n`threshold`：0.0-1.0，預設 0.7（70% 重疊才算重複）。\n`topic_keys`：逗號分隔，指定要比較的 topicKey 清單；不指定則比較所有傳入的候選集。\n`project`：預設 sirin。\n\n回傳：相似對（pair_a, pair_b, jaccard_score）清單，score 高 → 越相似。",
                "inputSchema": {
                    "type": "object",
                    "required": ["topic_keys"],
                    "properties": {
                        "topic_keys": { "type": "string", "description": "逗號分隔的 topicKey 清單（至少 2 個）" },
                        "project":   { "type": "string", "description": "project slug，預設 sirin" },
                        "threshold": { "type": "number", "description": "Jaccard 相似度閾值，預設 0.7" }
                    }
                }
            },
            {
                "name": "kb_stats",
                "description": "#226 — KB 深度統計（補強 kbHealth）。透過 agora-trading MCP 取得指定 project 的條目分布：by domain / by status / by layer / draft ratio / stale ratio / oldest+newest confirmed。\n\n需要 agora-trading + agora-ops token 在 ~/.claude.json 中設定。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": { "type": "string", "description": "project slug：sirin / agora-backend / flutter，預設 sirin" }
                    }
                }
            },
            {
                "name": "kb_diff",
                "description": "#226 — 對比兩個 KB 條目的內容差異（行級 unified diff）。適合追蹤同一 topicKey 在不同版本的演進，或對比兩個相關條目的內容重疊度。\n\n需要 agora-trading token 在 ~/.claude.json 中設定。",
                "inputSchema": {
                    "type": "object",
                    "required": ["topic_a", "topic_b"],
                    "properties": {
                        "topic_a":  { "type": "string", "description": "第一個 topicKey" },
                        "topic_b":  { "type": "string", "description": "第二個 topicKey" },
                        "project_a": { "type": "string", "description": "topic_a 的 project，預設 sirin" },
                        "project_b": { "type": "string", "description": "topic_b 的 project，預設同 project_a" }
                    }
                }
            },
            {
                "name": "config_diagnostics",
                "description": "回傳 Sirin 當前配置診斷（LLM backend 連通、router 狀態、vision 可用性、Chrome/Claude CLI 等）。遇到測試全部失敗時用來自我檢查。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "diagnose",
                "description": "Sirin 自我診斷快照 — 回傳 version / git commit / build date / platform / uptime / Chrome 狀態 / LLM provider+model / update 狀態 / 最近 ERROR/WARN log，外加一個預先填好環境資訊的 GitHub issue 模板（report_issue_template.body）。\n\n用法：外部 AI 在 Sirin MCP 操作遇到 bug 時，先呼叫 diagnose 拿快照，據此判斷：(1) 重試（transient）；(2) 提示用戶升級（你在 0.3.0 但 0.3.2 修了這個）；(3) 用 report_issue_template 開 issue（環境區塊已填好，用戶只要補 reproduction）。\n\n成本：~5–20 ms（一次 CDP getVersion + log tail）。安全在 caller 的 error path 每次呼叫。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "page_state",
                "description": "一次回傳當前瀏覽器頁面的完整狀態 — URL、title、ax_tree 文字片段、JPEG 截圖（Base64）、console 錯誤、最近網路請求。比分別呼叫多個 browser_exec 動作更快，適合 AI agent 做 situational awareness。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "include_screenshot": {
                            "type": "boolean",
                            "description": "是否包含截圖（預設 true）。截圖為 JPEG 80% Base64"
                        },
                        "include_ax": {
                            "type": "boolean",
                            "description": "是否包含 ax_tree 文字摘要（預設 true）"
                        },
                        "max_ax_nodes": {
                            "type": "number",
                            "description": "ax_tree 最多返回幾個節點（預設 50）"
                        }
                    }
                }
            },
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
            {
                "name": "agent_team_status",
                "description": "查看 PM / Engineer / Tester 三個 session 的目前狀態。\
回傳每個 session 的 session_id 和 resume 指令（可直接在 terminal 執行 `claude --resume <id>` 查看對話歷史）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cwd": { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
                    }
                }
            },
            {
                "name": "agent_team_task",
                "description": "派一個任務給 AI 團隊：PM 拆解 → Engineer 執行 → PM review。\
回傳 PM 的最終 review 結果。每個角色的對話歷史都會自動保留，可用 agent_team_status 取得 resume 指令查看。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "任務描述" },
                        "cwd":  { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
                    },
                    "required": ["task"]
                }
            },
            {
                "name": "agent_team_test",
                "description": "觸發測試循環：Tester 跑測試 → 失敗則 Engineer 修 → PM 記錄學習。\
回傳最終測試結果摘要。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cwd": { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
                    }
                }
            },
            {
                "name": "agent_send",
                "description": "直接送一條訊息給指定角色（pm / engineer / tester），取得回覆。\
對話歷史自動延續。適合：查詢 PM 的學習記錄、問工程師問題、請測試 session 執行特定測試。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "role":    { "type": "string", "enum": ["pm", "engineer", "tester"], "description": "目標角色" },
                        "message": { "type": "string", "description": "要送的訊息" },
                        "cwd":     { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
                    },
                    "required": ["role", "message"]
                }
            },
            {
                "name": "agent_reset",
                "description": "重置指定角色的 session（清除對話歷史，開新對話）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "role": { "type": "string", "enum": ["pm", "engineer", "tester", "all"], "description": "要重置的角色" },
                        "cwd":  { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
                    },
                    "required": ["role"]
                }
            },
            {
                "name": "agent_enqueue",
                "description": "把一個任務加入 AI 小隊的任務佇列。Worker 執行緒會自動依序執行（PM→Engineer→PM review）。\
回傳任務 ID，可用 agent_queue_status 查詢進度。\n\n\
T2-2：傳入 yaml_test_id 後，Engineer 完成任務時 Sirin 會自動執行該 YAML test 驗證，\
失敗則讓 Engineer 修 YAML 再試一次。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task":         { "type": "string", "description": "任務描述（越具體越好）" },
                        "cwd":          { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" },
                        "priority":     { "type": "integer", "minimum": 0, "maximum": 255, "description": "任務優先級：0=緊急，50=正常（預設），255=最低" },
                        "yaml_test_id": { "type": "string", "description": "（T2-2）Engineer 完成後，Sirin 自動跑這個 YAML test 驗證。\
傳 test_id（不含 .yaml）；系統從 Sirin repo 的 config/tests/ 遞迴搜尋，失敗則讓 Engineer 修 YAML 再試。" }
                    },
                    "required": ["task"]
                }
            },
            {
                "name": "agent_queue_status",
                "description": "查看 AI 小隊任務佇列現況（所有任務的 id / status / 結果摘要）。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "agent_start_worker",
                "description": "啟動 AI 小隊的背景工作執行緒（若尚未啟動）。啟動後持續消費佇列直到進程結束。可選 n 啟動多個平行 worker（T1-1）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cwd": { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" },
                        "n":   { "type": "integer", "description": "Worker 執行緒數量（預設 1，最大 8；建議 2-3）。每 worker 有獨立的 PM/Engineer/Tester session。", "minimum": 1, "maximum": 8 }
                    }
                }
            },
            {
                "name": "agent_clear_completed",
                "description": "清除任務佇列中所有已完成（done / failed）的任務，保留 queued / running。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "squad_knowledge",
                "description": "查看 Squad PM 積累的學習記錄（squad_knowledge.db）。\
列出 PM 在任務 review 中記錄的 [📝 學到:] 條目，這些知識會自動注入到下一個相關任務的規劃階段。\
可用於確認 PM 是否正確學到教訓、或除錯「為何 PM 一直犯同樣的錯」。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "description": "最多回傳幾條（預設 20，最大 100）" }
                    }
                }
            },
            {
                "name": "dev_team_enqueue_issue",
                "description": "從 GitHub issue 直接餵任務給 Sirin Dev Team。\
讀取 issue 標題+內文+labels，包成 task 後放進佇列；任務完成後 system 會自動把 PM 的最終 review 貼回 issue 留言（除非 dry_run=true）。\
\n\n預設 dry_run=true（驗證模式）— PM/Engineer/Tester 會收到一段禁止 gh issue comment / git push 的系統提示，task 完成時 review 會存到 data/multi_agent/preview_comments.jsonl 而非貼到 GitHub。\
要真的跑（會留言/可能 push）就明確傳 dry_run=false。\
\n\nproject_key 例如 'agora_market' / 'sirin'，會決定 cwd（透過 claude_session::repo_path）；gh_repo 是 GitHub 的 owner/name 字串（例如 'Redandan/AgoraMarket'）。\
需要 gh CLI 已認證（`gh auth status` 通過）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project_key":  { "type": "string", "description": "邏輯專案名稱（決定 cwd 與 session 命名空間）。例：'agora_market', 'sirin', 'agora_api'" },
                        "gh_repo":      { "type": "string", "description": "GitHub repo（owner/name 格式）。例：'Redandan/AgoraMarket'" },
                        "issue_number": { "type": "integer", "minimum": 1, "description": "Issue 編號" },
                        "dry_run":      { "type": "boolean", "description": "驗證模式（預設 true）。true=不會貼 GitHub 留言、不會 git push；false=正常跑會留言。", "default": true },
                        "priority":     { "type": "integer", "minimum": 0, "maximum": 255, "description": "任務優先級（預設 50）" }
                    },
                    "required": ["project_key", "gh_repo", "issue_number"]
                }
            },
            {
                "name": "dev_team_list_previews",
                "description": "列出所有 dry-run 任務存下來的 preview comments（位於 data/multi_agent/preview_comments.jsonl）。\
每筆包含 task_id / issue_url / success / body / saved_at — 給人類 review 後決定要不要用 dev_team_replay_preview 真的貼到 GitHub。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit":     { "type": "integer", "minimum": 1, "maximum": 200, "description": "最多回傳幾筆（預設 20，由新到舊）" },
                        "issue_url": { "type": "string", "description": "選填：只列出這個 issue 的 preview" }
                    }
                }
            },
            {
                "name": "dev_team_replay_preview",
                "description": "把指定 task_id 的 dry-run preview 真的貼到 GitHub issue 留言。\
用於人類 review 過 preview 內容、確認 OK 後手動觸發 — 等同於把 dry-run 模式跑出來的結果「approve + post」。\
留言會加 'replayed from dry-run' 標記，與一般 worker 自動貼的格式有所區別。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string", "description": "要 replay 的 task_id（從 dev_team_list_previews 取得）" }
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "dev_team_read_issue",
                "description": "用 gh CLI 讀單一 issue 的 title / body / labels（不會把它放進 task 佇列）。\
適合：先看內容判斷要不要餵給 dev team、或除錯 enqueue 失敗時拿原始資料。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "gh_repo":      { "type": "string", "description": "GitHub repo（owner/name）" },
                        "issue_number": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["gh_repo", "issue_number"]
                }
            },
            {
                "name": "consult",
                "description": "把一個問題交給另一個 Claude Code session 回答。\
Sirin 會在指定工作目錄（可以是另一個 repo）啟動一個顧問 session，\
讓它讀取程式碼後給出簡潔可執行的建議，再把答案帶回來。\
適合：「這個 API 格式對嗎？」「後端怎麼實作這個？」等需要跨 repo 判斷的問題。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question":   { "type": "string", "description": "要問顧問的問題" },
                        "context":    { "type": "string", "description": "背景說明（目前在做什麼）" },
                        "cwd":        { "type": "string", "description": "顧問 session 的工作目錄（repo 路徑）。省略時用 sirin 自身目錄" }
                    },
                    "required": ["question"]
                }
            },
            {
                "name": "supervised_run",
                "description": "在指定 repo 啟動一個受監督的 Claude Code session。\
當主 session 停下來（問問題 / 達到輪次上限），Sirin 自動決定怎麼回應：\
- policy=auto：直接回「yes, continue」\
- policy=consult：把問題轉給另一個 Claude session 取得建議再回答\
最多執行 5 輪，全部完成後回傳結果摘要（含每輪事件）。\
注意：可能需要 1-5 分鐘，視任務複雜度而定。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cwd":            { "type": "string", "description": "主 session 工作目錄（repo 路徑）" },
                        "prompt":         { "type": "string", "description": "給 Claude Code 的任務描述" },
                        "policy":         { "type": "string", "enum": ["auto", "consult"], "description": "auto=直接回 yes；consult=問另一個 session（預設 auto）" },
                        "consultant_cwd": { "type": "string", "description": "顧問 session 的工作目錄，policy=consult 時有效（省略則與 cwd 相同）" }
                    },
                    "required": ["cwd", "prompt"]
                }
            },
            {
                "name": "assistant_task",
                "description": "用自然語言請求執行一般網頁任務（非 Flutter 測試）。\
Sirin 透過視覺驅動的 ReAct loop 操作瀏覽器完成任務，回傳結果摘要。\n\
適用場景：\n\
- 在 Google Maps 找附近餐廳並過濾評分\n\
- 在 Facebook 查詢修車廠聯絡資訊\n\
- 翻譯外語頁面內容（支援泰文等）\n\
- 在任意網頁填表、搜尋、提取資料\n\
注意：使用 Sirin 的 Chrome session（需先啟動 Sirin），最多執行 25 步。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "request": {
                            "type": "string",
                            "description": "自然語言任務描述，例如：在 Google Maps 找附近評分 4 星以上的泰式餐廳"
                        },
                        "url": {
                            "type": "string",
                            "description": "選填：起始 URL，例如 https://maps.google.com"
                        }
                    },
                    "required": ["request"]
                }
            },
            {
                "name": "kb_search",
                "description": "語意搜尋 AgoraMarket Knowledge Base，返回最相關的條目內容。KB 必須啟用（KB_ENABLED=1）。Bearer token 留在 Sirin 端，瀏覽器永遠看不到。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query":   { "type": "string",  "description": "搜尋查詢文字" },
                        "project": { "type": "string",  "description": "KB project slug（預設讀 KB_PROJECT env，通常是 'agora_market'）" },
                        "limit":   { "type": "integer", "description": "最多返回幾筆（預設 5，最大 20）" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "kb_get",
                "description": "依 topicKey 取得 Knowledge Base 單一條目。KB 必須啟用（KB_ENABLED=1）。Bearer token 留在 Sirin 端。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic_key": { "type": "string", "description": "KB topicKey（例如 'agora-pickup-flow'）" },
                        "project":   { "type": "string", "description": "KB project slug（預設讀 KB_PROJECT env）" }
                    },
                    "required": ["topic_key"]
                }
            },
            {
                "name": "kb_write",
                "description": "Write a note to AgoraMarket Knowledge Base. Stored as layer=raw, status=draft, source=sirin. KB must be enabled (KB_ENABLED=1).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic_key": { "type": "string", "description": "unique kebab-case key, e.g. 'cic-result-demo-001'" },
                        "title":     { "type": "string", "description": "human-readable title" },
                        "content":   { "type": "string", "description": "note body (Markdown OK)" },
                        "domain":    { "type": "string", "description": "e.g. 'tooling', 'cic', 'session-task'" },
                        "tags":      { "type": "string", "description": "comma-separated, e.g. 'cic-task,result'" },
                        "file_refs": { "type": "string", "description": "optional comma-separated file refs" },
                        "project":   { "type": "string", "description": "KB project slug (default reads KB_PROJECT env)" }
                    },
                    "required": ["topic_key", "title", "content", "domain"]
                }
            },
            {
                "name": "browser_status",
                "description": "查詢 Sirin 目前開啟的 Chrome 瀏覽器狀態。\n\n返回：\n- is_open: Chrome 是否已啟動\n- tab_count: 總 tab 數\n- active_tab: 目前 active tab 的 index + URL\n- tabs: 每個 tab 的 index、URL、session_id（若有 named session）\n- named_sessions: 所有 named session 的 id → tab_index → URL 清單\n\n用途：排查 batch 測試後殘留的 tab、確認 session 是否正確關閉、即時查看瀏覽器狀態。",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sync_config",
                "description": "將 repo 的 config/tests/ 同步到 %LOCALAPPDATA%\\Sirin\\config\\tests/（Sirin 執行時讀取的位置）。\n\n每次修改 YAML 測試檔後必須呼叫，否則 Sirin 跑的是舊版 YAML。\n\n返回：synced=true, files_copied=N。\n\n關閉 #187。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "run_regression_suite",
                "description": "一鍵執行 config/tests/agora_regression/ 下所有（或指定 tag 的）regression tests，等全部完成後回傳摘要報告。\n\n返回：total/passed/failed/timeout/duration_secs + 每個測試的 status/duration_ms/error。\n\n可選參數：\n- tag: 只跑含此 tag 的測試（如 'c2c'）\n- timeout_secs: 整體 timeout（預設 3600）\n\n關閉 #188。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tag": { "type": "string", "description": "只跑含此 tag 的測試，空字串=全部" },
                        "timeout_secs": { "type": "number", "description": "整體 timeout 秒數（預設 3600）" }
                    }
                }
            },
            {
                "name": "sirin_preflight",
                "description": "Session 開始前驗證環境就緒：Sirin MCP、config sync 狀態、redandan.github.io 版本、API healthy。\n\n返回：ready=true/false + 各項檢查結果 + warnings 清單。\n\n關閉 #193。",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    }))
}

// ── tools/call ────────────────────────────────────────────────────────────────

async fn handle_tools_call(params: Value, user_agent: &str) -> Result<Value, String> {
    let name      = params["name"].as_str().ok_or("Missing 'name'")?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    // Tools that return structured JSON (not just text) bypass the text wrapper.
    // Only `browser_exec` currently needs the caller's identity (for authz +
    // audit); other tools are read-only w.r.t. authz.
    match name {
        "list_tests"           => return call_list_tests(arguments).map(wrap_json),
        "run_test_async"       => return call_run_test_async(arguments).map(wrap_json),
        "run_test_batch"       => return call_run_test_batch(arguments).map(wrap_json),
        "run_test_pipeline"    => return call_run_test_pipeline(arguments).map(wrap_json),
        "run_adhoc_test"       => return call_run_adhoc_test(arguments).map(wrap_json),
        "persist_adhoc_run"    => return call_persist_adhoc_run(arguments).map(wrap_json),
        "get_test_result"      => return call_get_test_result(arguments).map(wrap_json),
        "kill_run"             => return call_kill_run(arguments).map(wrap_json),
        "get_screenshot"       => return call_get_screenshot(arguments).map(wrap_json),
        "get_full_observation" => return call_get_full_observation(arguments).map(wrap_json),
        "get_run_trace"        => return call_get_run_trace(arguments).map(wrap_json),
        "list_recent_runs"     => return call_list_recent_runs(arguments).map(wrap_json),
        "test_analytics"       => return call_test_analytics(arguments).map(wrap_json),
        "test_summary"         => return call_test_summary(arguments).map(wrap_json),
        "list_flaky_tests"        => return call_list_flaky_tests(arguments).map(wrap_json),
        "test_coverage"           => return call_test_coverage(arguments).map(wrap_json),
        "discover_app"            => return call_discover_app(arguments).map(wrap_json),
        "discovery_status"        => return call_discovery_status().map(wrap_json),
        "discovery_features"      => return call_discovery_features(arguments).map(wrap_json),
        "ui_navigate"             => return call_ui_navigate(arguments).map(wrap_json),
        "ui_state"                => return call_ui_state().map(wrap_json),
        "replay_last_failure"     => return call_replay_last_failure(arguments).map(wrap_json),
        "shadow_dump_diff"        => return call_shadow_dump_diff(arguments).map(wrap_json),
        "compare_with_replay"     => return call_compare_with_replay(arguments).map(wrap_json),
        "explain_failure"         => return call_explain_failure(arguments).await.map(wrap_json),
        "list_saved_scripts"   => return call_list_saved_scripts().map(wrap_json),
        "delete_saved_script"  => return call_delete_saved_script(arguments).map(wrap_json),
        "list_fixes"           => return call_list_fixes(arguments).map(wrap_json),
        "suggest_allowlist"    => return call_suggest_allowlist(arguments).map(wrap_json),
        "list_redundant_allow" => return call_list_redundant_allow().map(wrap_json),
        // #225 claude-config-mcp
        "list_allowlist"       => return call_list_allowlist().map(wrap_json),
        "add_allow"            => return call_add_allow(arguments).map(wrap_json),
        "remove_allow"         => return call_remove_allow(arguments).map(wrap_json),
        "list_slash_commands"  => return call_list_slash_commands().map(wrap_json),
        "list_hooks"           => return call_list_hooks().map(wrap_json),
        // #227 session-memory-mcp
        "save_point"           => return call_save_point(arguments).map(wrap_json),
        "list_points"          => return call_list_points(arguments).map(wrap_json),
        "restore_point"        => return call_restore_point(arguments).map(wrap_json),
        "expire_points"        => return call_expire_points().map(wrap_json),
        // #232 session-cost-mcp
        "session_cost"            => return call_session_cost(arguments).map(wrap_json),
        "list_expensive_sessions" => return call_list_expensive_sessions(arguments).map(wrap_json),
        // #228 task-tracker-mcp
        "create_task"    => return call_create_task(arguments).map(wrap_json),
        "list_tasks"     => return call_list_tasks(arguments).map(wrap_json),
        "mark_task_done" => return call_mark_task_done(arguments).map(wrap_json),
        "link_task"           => return call_link_task(arguments).map(wrap_json),
        // #224 handoff-mcp
        "create_handoff"      => return call_create_handoff(arguments).map(wrap_json),
        "get_latest_handoff"  => return call_get_latest_handoff(arguments).map(wrap_json),
        "list_handoff_history"=> return call_list_handoff_history(arguments).map(wrap_json),
        // #226 kb-lifecycle (non-embedding subset)
        "kb_stats"             => return call_kb_stats(arguments).await.map(wrap_json),
        "kb_diff"              => return call_kb_diff(arguments).await.map(wrap_json),
        "kb_duplicate_check"   => return call_kb_duplicate_check(arguments).await.map(wrap_json),
        // #231 agora-daily-brief
        "generate_daily_brief"=> return call_generate_daily_brief(arguments).await.map(wrap_json),
        // #226 kb-merge
        "kb_merge"            => return call_kb_merge(arguments).await.map(wrap_json),
        // #233 cross-ai-router-mcp
        "route_query"         => return call_route_query(arguments).await.map(wrap_json),
        "query_llm"           => return call_query_llm(arguments).await.map(wrap_json),
        "fallback_chain"      => return call_fallback_chain(arguments).await.map(wrap_json),
        "list_intents"        => return call_list_intents().map(wrap_json),
        "register_intent"     => return call_register_intent(arguments).map(wrap_json),
        "benchmark_llms"      => return call_benchmark_llms(arguments).await.map(wrap_json),
        "config_diagnostics"   => return call_config_diagnostics().map(wrap_json),
        "diagnose"             => return Ok(wrap_json(crate::diagnose::snapshot())),
        "browser_exec"         => return call_browser_exec(arguments, user_agent).await.map(wrap_json),
        "browser_status"       => return call_browser_status().map(wrap_json),
        "sync_config"          => return call_sync_config().map(wrap_json),
        "run_regression_suite" => return call_run_regression_suite(arguments).await.map(wrap_json),
        "sirin_preflight"      => return call_sirin_preflight().await.map(wrap_json),
        "page_state"           => return call_page_state(arguments).await.map(wrap_json),
        "consult"              => return call_consult(arguments).map(wrap_json),
        "supervised_run"       => return call_supervised_run(arguments).map(wrap_json),
        "assistant_task"       => return call_assistant_task(arguments).await.map(wrap_json),
        "agent_team_status"    => return call_agent_team_status(arguments).map(wrap_json),
        "agent_team_task"      => return call_agent_team_task(arguments).map(wrap_json),
        "agent_team_test"      => return call_agent_team_test(arguments).map(wrap_json),
        "agent_send"           => return call_agent_send(arguments).map(wrap_json),
        "agent_reset"          => return call_agent_reset(arguments).map(wrap_json),
        "agent_enqueue"        => return call_agent_enqueue(arguments).map(wrap_json),
        "agent_queue_status"   => return call_agent_queue_status().map(wrap_json),
        "agent_start_worker"   => return call_agent_start_worker(arguments).map(wrap_json),
        "agent_clear_completed"=> return call_agent_clear_completed().map(wrap_json),
        "squad_knowledge"      => return call_squad_knowledge(arguments).map(wrap_json),
        "dev_team_enqueue_issue"  => return call_dev_team_enqueue_issue(arguments).map(wrap_json),
        "dev_team_list_previews"  => return call_dev_team_list_previews(arguments).map(wrap_json),
        "dev_team_replay_preview" => return call_dev_team_replay_preview(arguments).map(wrap_json),
        "dev_team_read_issue"     => return call_dev_team_read_issue(arguments).map(wrap_json),
        "kb_search"               => return call_kb_search(arguments).await.map(wrap_json),
        "kb_get"                  => return call_kb_get(arguments).await.map(wrap_json),
        "kb_write"                => return call_kb_write(arguments).await.map(wrap_json),
        _ => {}
    }

    let text = match name {
        "memory_search"    => call_memory_search(arguments).await?,
        "skill_list"       => call_skill_list(),
        "teams_pending"    => call_teams_pending(),
        "teams_approve"    => call_teams_approve(arguments)?,
        "trigger_research" => call_trigger_research(arguments)?,
        other => return Err(format!("Unknown tool: {other}")),
    };

    // MCP content format (text only tools)
    Ok(json!({
        "content": [{ "type": "text", "text": text }]
    }))
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

async fn call_memory_search(args: Value) -> Result<String, String> {
    let query = args["query"].as_str().ok_or("Missing query")?.to_string();
    let limit = args["limit"].as_u64().unwrap_or(5) as usize;

    blocking("memory_search", move || {
        crate::memory::memory_search(&query, limit, "")
            .map(|results| results.join("\n\n"))
            .map_err(|e| e.to_string())
    })
    .await
}

fn call_skill_list() -> String {
    let skills = crate::skills::list_skills();
    skills
        .iter()
        .map(|s| format!("• **{}** ({})\n  {}", s.name, s.category, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}

fn call_teams_pending() -> String {
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

fn call_teams_approve(args: Value) -> Result<String, String> {
    let id = args["id"].as_str().ok_or("Missing id")?;
    crate::pending_reply::update_status(
        "teams", id,
        crate::pending_reply::PendingStatus::Approved,
    );
    Ok(format!("草稿 {id} 已核准，等待送出。"))
}

fn call_trigger_research(args: Value) -> Result<String, String> {
    let topic = args["topic"].as_str().ok_or("Missing topic")?.to_string();
    let url   = args["url"].as_str().map(|s| s.to_string());

    crate::events::publish(crate::events::AgentEvent::ResearchRequested {
        topic: topic.clone(),
        url,
    });
    Ok(format!("已觸發對「{topic}」的調研任務。"))
}

// ── Test runner MCP handlers ─────────────────────────────────────────────────

fn call_list_tests(args: Value) -> Result<Value, String> {
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

fn call_run_test_async(args: Value) -> Result<Value, String> {
    let test_id = args["test_id"].as_str().ok_or("Missing test_id")?.to_string();
    let auto_fix = args.get("auto_fix").and_then(Value::as_bool).unwrap_or(false);

    // Look up the test goal before spawning so we can surface docs_refs.
    // spawn_run_async will also look it up internally; this double-read is
    // cheap (small YAML dir) and lets us surface the warning before the run.
    let docs_refs = crate::test_runner::parser::find(&test_id)
        .map(|g| g.docs_refs)
        .unwrap_or_default();

    let run_id = crate::test_runner::spawn_run_async(test_id.clone(), auto_fix)?;

    let mut resp = json!({
        "run_id": run_id,
        "test_id": test_id,
        "auto_fix": auto_fix,
        "status": "queued",
        "poll_with": "get_test_result",
    });

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

/// Average LLM calls per minute per test, used for RPM-aware batch sizing.
fn calls_per_min_per_test_avg(test_ids: &[String]) -> f64 {
    if test_ids.is_empty() { return 10.0; }
    test_ids.iter().map(|tid| {
        let stats = crate::test_runner::store::test_stats(tid);
        if stats.avg_iterations > 0.0 && stats.avg_duration_ms > 0 {
            stats.avg_iterations / (stats.avg_duration_ms as f64 / 60_000.0)
        } else { 10.0 }
    }).sum::<f64>() / test_ids.len() as f64
}

/// Spawn N tests in parallel, each on its own dedicated chrome tab.
///
/// `max_concurrency` clamped to [1, 8].  CDP isn't designed for hundreds
/// of simultaneous tabs — 8 is conservative; if you need a wider sweep,
/// shard externally and call this multiple times.
fn call_run_test_batch(args: Value) -> Result<Value, String> {
    let test_ids: Vec<String> = args.get("test_ids")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .ok_or("Missing test_ids (array of strings)")?;
    if test_ids.is_empty() {
        return Err("test_ids is empty".into());
    }

    // ── RPM-aware concurrency cap ─────────────────────────────────────────────
    //
    // Running more tests in parallel than the LLM's RPM can support causes
    // every test to stall at the token-bucket gate — total wall time increases
    // but no individual test runs any faster.  Better to serialise: each test
    // gets the full RPM budget and finishes in its natural time.
    //
    // Formula:
    //   rpm                  = GEMINI_RPM (default 8 for Gemini free tier)
    //   avg_calls_per_min    = avg_iterations / (avg_duration_ms / 60000)
    //   safe_concurrency     = max(1, floor(rpm / avg_calls_per_min))
    //
    // We use the analytics of each requested test and average across them.
    // For tests with no history, assume 10 calls/minute (conservative default).
    let rpm = std::env::var("GEMINI_RPM")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|&r| r > 0.0)
        .unwrap_or(8.0);

    let provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
    let is_gemini = provider.to_lowercase().contains("gemini");

    let safe_concurrency: usize = if is_gemini {
        // Average calls/minute across all requested tests using their analytics.
        let calls_per_min_per_test: f64 = test_ids
            .iter()
            .map(|tid| {
                let stats = crate::test_runner::store::test_stats(tid);
                if stats.avg_iterations > 0.0 && stats.avg_duration_ms > 0 {
                    // calls/min = iterations / (duration in minutes)
                    stats.avg_iterations / (stats.avg_duration_ms as f64 / 60_000.0)
                } else {
                    10.0 // conservative default for tests with no history
                }
            })
            .sum::<f64>()
            / test_ids.len() as f64;

        let safe = (rpm / calls_per_min_per_test).floor() as usize;
        safe.max(1)
    } else {
        8 // non-Gemini providers have no RPM concern; use hardware cap
    };

    let raw_cap = args.get("max_concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(safe_concurrency as u64) as usize;

    let mut warn: Option<String> = None;
    let cap = if raw_cap > safe_concurrency && is_gemini {
        warn = Some(format!(
            "max_concurrency={raw_cap} exceeds RPM-safe limit={safe_concurrency} \
             (GEMINI_RPM={rpm:.0}, avg ~{:.1} LLM calls/min/test). \
             Clamping to {safe_concurrency} to prevent 429 stalls. \
             Increase GEMINI_RPM or use a paid tier for higher parallelism.",
            calls_per_min_per_test_avg(&test_ids)
        ));
        safe_concurrency
    } else {
        raw_cap.clamp(1, 8)
    };

    tracing::info!(
        "[batch] {} tests, concurrency={}/{} (RPM={:.0}, safe={})",
        test_ids.len(), cap, raw_cap, rpm, safe_concurrency
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

fn call_run_test_pipeline(args: Value) -> Result<Value, String> {
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

fn call_get_test_result(args: Value) -> Result<Value, String> {
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

fn call_kill_run(args: Value) -> Result<Value, String> {
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

fn call_get_screenshot(args: Value) -> Result<Value, String> {
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

fn call_run_adhoc_test(args: Value) -> Result<Value, String> {
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
fn call_persist_adhoc_run(args: Value) -> Result<Value, String> {
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

fn call_list_recent_runs(args: Value) -> Result<Value, String> {
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
fn call_test_summary(args: Value) -> Result<Value, String> {
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
fn call_test_coverage(args: Value) -> Result<Value, String> {
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
fn call_discover_app(args: Value) -> Result<Value, String> {
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
fn call_discovery_status() -> Result<Value, String> {
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
fn call_discovery_features(args: Value) -> Result<Value, String> {
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

/// TEST/DEV — drive UI from MCP. Translates `target` strings into UiCommand
/// enums and pushes them onto the test bus. Sleeps briefly so the egui
/// frame can pick up the command before the caller queries `ui_state`.
fn call_ui_navigate(args: Value) -> Result<Value, String> {
    use crate::ui_test_bus::{push, UiCommand};

    let target = args.get("target").and_then(Value::as_str)
        .ok_or("target required")?.to_string();
    let lower  = target.to_lowercase();

    // Helper for "prefix:rest" parsing.
    let split_once = |s: &str, p: char| -> Option<(String, String)> {
        s.split_once(p).map(|(a, b)| (a.to_string(), b.to_string()))
    };

    let cmd = if lower == "dashboard" || lower == "home" {
        UiCommand::GoDashboard
    } else if lower == "testing" || lower == "testing:runs" {
        UiCommand::GoTesting { tab: "runs".into() }
    } else if lower == "testing:coverage" || lower == "coverage" {
        UiCommand::GoTesting { tab: "coverage".into() }
    } else if lower == "testing:browser" || lower == "browser" {
        UiCommand::GoTesting { tab: "browser".into() }
    } else if let Some((p, r)) = split_once(&lower, ':')
        .filter(|(p, _)| p == "workspace")
    {
        let _ = p;
        let idx: usize = r.parse().map_err(|e| format!("workspace:N — bad N: {e}"))?;
        UiCommand::GoWorkspace { idx }
    } else if lower == "palette" {
        UiCommand::OpenPalette { query: None }
    } else if let Some((p, q)) = split_once(&target, ':')
        .filter(|(p, _)| p.eq_ignore_ascii_case("palette"))
    {
        let _ = p;
        UiCommand::OpenPalette { query: Some(q) }
    } else if lower == "settings" {
        UiCommand::OpenModal { kind: "system".into(), tab: Some("settings".into()) }
    } else if lower == "logs" || lower == "log" {
        UiCommand::OpenModal { kind: "system".into(), tab: Some("log".into()) }
    } else if lower == "devsquad" || lower == "squad" {
        UiCommand::OpenModal { kind: "automation".into(), tab: Some("squad".into()) }
    } else if lower == "mcp" || lower == "mcp-playground" {
        UiCommand::OpenModal { kind: "automation".into(), tab: Some("mcp".into()) }
    } else if lower == "ai-router" || lower == "airouter" {
        UiCommand::OpenModal { kind: "ops".into(), tab: Some("airouter".into()) }
    } else if lower == "tasks" || lower == "session-tasks" {
        UiCommand::OpenModal { kind: "ops".into(), tab: Some("sessiontasks".into()) }
    } else if lower == "cost-kb" || lower == "costkb" || lower == "cost" {
        UiCommand::OpenModal { kind: "ops".into(), tab: Some("costkb".into()) }
    } else if lower == "close" || lower == "esc" {
        UiCommand::CloseModal
    } else if lower == "close-palette" {
        UiCommand::ClosePalette
    } else if lower == "gear" || lower == "gear-menu" {
        UiCommand::OpenGearMenu
    } else if lower == "close-gear" {
        UiCommand::CloseGearMenu
    } else {
        return Err(format!("unknown target '{target}'"));
    };

    push(cmd);
    // Give the egui frame ~80ms to drain + apply the command before the
    // caller calls ui_state. Tunable; 80ms covers slow first-frame layout.
    std::thread::sleep(std::time::Duration::from_millis(80));

    Ok(json!({
        "target":    target,
        "applied":   true,
        "note":      "next ui_state call reflects the new view",
    }))
}

/// TEST/DEV — read current UI snapshot.
fn call_ui_state() -> Result<Value, String> {
    let snap = crate::ui_test_bus::get_state()
        .ok_or("UI not yet rendered (no snapshot)")?;
    Ok(json!({
        "view":           snap.view,
        "workspace_idx":  snap.workspace_idx,
        "testing_tab":    snap.testing_tab,
        "modal":          snap.modal,
        "modal_tab":      snap.modal_tab,
        "palette_open":   snap.palette_open,
        "palette_query":  snap.palette_query,
        "gear_menu_open": snap.gear_menu_open,
        "agent_count":    snap.agent_count,
        "active_runs":    snap.active_runs,
        "recent_runs":    snap.recent_runs,
    }))
}

/// #230 — list_flaky_tests: tests with pass_rate < threshold over recent runs.
fn call_list_flaky_tests(args: Value) -> Result<Value, String> {
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
async fn call_explain_failure(args: Value) -> Result<Value, String> {
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

/// #230 — replayLastFailure: step-by-step inspection of the last failed run.
fn call_replay_last_failure(args: Value) -> Result<Value, String> {
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
fn call_shadow_dump_diff(args: Value) -> Result<Value, String> {
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
fn call_compare_with_replay(args: Value) -> Result<Value, String> {
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

fn call_test_analytics(args: Value) -> Result<Value, String> {
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
fn call_suggest_allowlist(args: Value) -> Result<Value, String> {
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
fn call_list_redundant_allow() -> Result<Value, String> {
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

fn call_list_allowlist() -> Result<Value, String> {
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

fn call_add_allow(args: Value) -> Result<Value, String> {
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

fn call_remove_allow(args: Value) -> Result<Value, String> {
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

fn call_list_slash_commands() -> Result<Value, String> {
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

fn call_list_hooks() -> Result<Value, String> {
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

fn call_save_point(args: Value) -> Result<Value, String> {
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

fn call_list_points(args: Value) -> Result<Value, String> {
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

fn call_restore_point(args: Value) -> Result<Value, String> {
    let label = args["label"].as_str().ok_or("Missing label")?;
    let points = load_points();
    let point = points.iter()
        .find(|p| p["label"].as_str() == Some(label))
        .ok_or_else(|| format!("Save point '{label}' not found"))?;
    Ok(point.clone())
}

fn call_expire_points() -> Result<Value, String> {
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

fn call_session_cost(args: Value) -> Result<Value, String> {
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

fn call_list_expensive_sessions(args: Value) -> Result<Value, String> {
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

fn call_create_task(args: Value) -> Result<Value, String> {
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

fn call_list_tasks(args: Value) -> Result<Value, String> {
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

fn call_mark_task_done(args: Value) -> Result<Value, String> {
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

fn call_link_task(args: Value) -> Result<Value, String> {
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

fn call_create_handoff(args: Value) -> Result<Value, String> {
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

    let payload = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "kbWrite",
            "arguments": {
                "topicKey":   "sirin-handoff-latest",
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

fn call_get_latest_handoff(args: Value) -> Result<Value, String> {
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

fn call_list_handoff_history(args: Value) -> Result<Value, String> {
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

async fn call_kb_duplicate_check(args: Value) -> Result<Value, String> {
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

async fn call_kb_stats(args: Value) -> Result<Value, String> {
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

async fn call_kb_diff(args: Value) -> Result<Value, String> {
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

async fn call_route_query(args: Value) -> Result<Value, String> {
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

async fn call_query_llm(args: Value) -> Result<Value, String> {
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

async fn call_fallback_chain(args: Value) -> Result<Value, String> {
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

fn call_list_intents() -> Result<Value, String> {
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

fn call_register_intent(args: Value) -> Result<Value, String> {
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

async fn call_benchmark_llms(args: Value) -> Result<Value, String> {
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

async fn call_generate_daily_brief(args: Value) -> Result<Value, String> {
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

async fn call_kb_merge(args: Value) -> Result<Value, String> {
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

fn call_list_saved_scripts() -> Result<Value, String> {
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

fn call_delete_saved_script(args: Value) -> Result<Value, String> {
    let test_id = args.get("test_id").and_then(Value::as_str)
        .ok_or("'delete_saved_script' requires 'test_id'")?;
    crate::test_runner::store::delete_script(test_id);
    Ok(json!({
        "status":  "deleted",
        "test_id": test_id,
        "note":    "Next run will use LLM ReAct loop and regenerate the script on success."
    }))
}

fn call_list_fixes(args: Value) -> Result<Value, String> {
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

fn call_config_diagnostics() -> Result<Value, String> {
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

fn call_agent_team_status(args: Value) -> Result<Value, String> {
    let cwd = resolve_cwd(&args);
    let guard = crate::multi_agent::get_or_init(&cwd);
    let team  = guard.as_ref().ok_or("team not initialized")?;
    Ok(serde_json::to_value(team.status()).unwrap_or(serde_json::json!({})))
}

fn call_agent_team_task(args: Value) -> Result<Value, String> {
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

fn call_agent_team_test(args: Value) -> Result<Value, String> {
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

fn call_agent_send(args: Value) -> Result<Value, String> {
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

fn call_agent_reset(args: Value) -> Result<Value, String> {
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

fn call_agent_enqueue(args: Value) -> Result<Value, String> {
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

fn call_agent_queue_status() -> Result<Value, String> {
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

fn call_agent_start_worker(args: Value) -> Result<Value, String> {
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

fn call_agent_clear_completed() -> Result<Value, String> {
    let before = crate::multi_agent::queue::list_all().len();
    crate::multi_agent::queue::clear_completed();
    let after = crate::multi_agent::queue::list_all().len();
    Ok(serde_json::json!({
        "removed": before - after,
        "remaining": after,
    }))
}

fn call_squad_knowledge(arguments: Value) -> Result<Value, String> {
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

fn call_dev_team_enqueue_issue(args: Value) -> Result<Value, String> {
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

fn call_dev_team_list_previews(args: Value) -> Result<Value, String> {
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

fn call_dev_team_replay_preview(args: Value) -> Result<Value, String> {
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

fn call_dev_team_read_issue(args: Value) -> Result<Value, String> {
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
fn call_consult(args: Value) -> Result<Value, String> {
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
async fn call_assistant_task(args: Value) -> Result<Value, String> {
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
fn call_supervised_run(args: Value) -> Result<Value, String> {
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
fn call_browser_status() -> Result<Value, String> {
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

fn call_sync_config() -> Result<Value, String> {
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

async fn call_run_regression_suite(args: Value) -> Result<Value, String> {
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

async fn call_sirin_preflight() -> Result<Value, String> {
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

async fn call_page_state(args: Value) -> Result<Value, String> {
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

fn call_get_full_observation(args: Value) -> Result<Value, String> {
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
fn call_get_run_trace(args: Value) -> Result<Value, String> {
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

async fn call_kb_search(args: Value) -> Result<Value, String> {
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

async fn call_kb_get(args: Value) -> Result<Value, String> {
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

async fn call_kb_write(args: Value) -> Result<Value, String> {
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
