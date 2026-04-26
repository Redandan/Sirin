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
    routing::post,
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
    Router::new()
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
        "run_adhoc_test"       => return call_run_adhoc_test(arguments).map(wrap_json),
        "persist_adhoc_run"    => return call_persist_adhoc_run(arguments).map(wrap_json),
        "get_test_result"      => return call_get_test_result(arguments).map(wrap_json),
        "get_screenshot"       => return call_get_screenshot(arguments).map(wrap_json),
        "get_full_observation" => return call_get_full_observation(arguments).map(wrap_json),
        "get_run_trace"        => return call_get_run_trace(arguments).map(wrap_json),
        "list_recent_runs"     => return call_list_recent_runs(arguments).map(wrap_json),
        "test_analytics"       => return call_test_analytics(arguments).map(wrap_json),
        "list_fixes"           => return call_list_fixes(arguments).map(wrap_json),
        "config_diagnostics"   => return call_config_diagnostics().map(wrap_json),
        "diagnose"             => return Ok(wrap_json(crate::diagnose::snapshot())),
        "browser_exec"         => return call_browser_exec(arguments, user_agent).await.map(wrap_json),
        "page_state"           => return call_page_state(arguments).await.map(wrap_json),
        "consult"              => return call_consult(arguments).map(wrap_json),
        "supervised_run"       => return call_supervised_run(arguments).map(wrap_json),
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
    let raw_cap = args.get("max_concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(3) as usize;
    let cap = raw_cap.clamp(1, 8);

    let run_ids = crate::test_runner::spawn_batch_run(test_ids.clone(), cap)?;
    // Pair each input test with its assigned run_id for client clarity.
    let pairs: Vec<Value> = test_ids.iter().zip(run_ids.iter())
        .map(|(tid, rid)| json!({ "test_id": tid, "run_id": rid }))
        .collect();
    Ok(json!({
        "count": pairs.len(),
        "max_concurrency": cap,
        "runs": pairs,
        "status": "queued",
        "poll_each_with": "get_test_result",
    }))
}

fn call_get_test_result(args: Value) -> Result<Value, String> {
    let run_id = args["run_id"].as_str().ok_or("Missing run_id")?;
    match crate::test_runner::runs::get(run_id) {
        Some(state) => Ok(crate::test_runner::runs::to_json(&state)),
        None => Err(format!("run_id '{run_id}' not found (may have been pruned)")),
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
    let items: Vec<Value> = runs.into_iter().map(|r| json!({
        "id":               r.id,
        "test_id":          r.test_id,
        "started_at":       r.started_at,
        "duration_ms":      r.duration_ms,
        "status":           r.status,
        "failure_category": r.failure_category,
        "ai_analysis":      r.ai_analysis,
        "screenshot_path":  r.screenshot_path,
    })).collect();
    Ok(json!({ "count": items.len(), "runs": items }))
}

fn call_test_analytics(args: Value) -> Result<Value, String> {
    let test_id = args.get("test_id").and_then(Value::as_str);
    let stats = match test_id {
        Some(tid) => vec![crate::test_runner::store::test_stats(tid)],
        None      => crate::test_runner::store::all_test_stats(),
    };
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
    })).collect();
    Ok(json!({
        "tests":   items,
        "summary": {
            "total_tests":   total_tests,
            "flaky_count":   flaky_count,
            "avg_pass_rate": avg_pass_rate,
        }
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
    let target = args.get("target").and_then(Value::as_str).unwrap_or("").to_string();
    let text   = args.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    let timeout = args.get("timeout").and_then(Value::as_u64);
    let headless_override = args.get("browser_headless").and_then(Value::as_bool);
    // Coord args for click_point; viewport args for set_viewport.  Fixes #11.
    let x = args.get("x").and_then(Value::as_f64);
    let y = args.get("y").and_then(Value::as_f64);
    let width = args.get("width").and_then(Value::as_u64);
    let height = args.get("height").and_then(Value::as_u64);
    let device_scale = args.get("device_scale").and_then(Value::as_f64).unwrap_or(1.0);
    let mobile = args.get("mobile").and_then(Value::as_bool).unwrap_or(false);
    // ax_* args
    let backend_id = args.get("backend_id").and_then(Value::as_u64).map(|n| n as u32);
    let role_arg = args.get("role").and_then(Value::as_str).map(String::from);
    let name_arg = args.get("name").and_then(Value::as_str).map(String::from);
    let include_ignored = args.get("include_ignored").and_then(Value::as_bool).unwrap_or(false);
    // Issue #19 new args
    let session_id_arg = args.get("session_id").and_then(Value::as_str).map(String::from);
    let scroll_arg = args.get("scroll").and_then(Value::as_bool).unwrap_or(false);
    let scroll_max_arg = args.get("scroll_max").and_then(Value::as_u64).unwrap_or(10) as usize;
    let min_nodes_arg = args.get("min_nodes").and_then(Value::as_u64).unwrap_or(20) as usize;
    let idle_ms_arg = args.get("idle_ms").and_then(Value::as_u64).unwrap_or(500);

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

    // Dispatch directly to crate::browser to avoid requiring an AgentContext
    // for simple imperative calls.  Mirrors the web_navigate action set.
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

        match action.as_str() {
            "goto" => {
                if target.is_empty() { return Err("'goto' requires 'target' URL".into()); }
                browser::ensure_open(want_headless)?;
                browser::navigate(&target)?;
                Ok(json!({ "status": "navigated", "url": target }))
            }
            "screenshot" => {
                let png = browser::screenshot()?;
                let b64 = base64_encode(&png);
                let url = browser::current_url().unwrap_or_default();
                Ok(json!({
                    "mime": "image/png",
                    "bytes_base64": b64,
                    "size_bytes": png.len(),
                    "url": url,
                }))
            }
            "click" => {
                if target.is_empty() { return Err("'click' requires 'target' selector".into()); }
                browser::click(&target)?;
                Ok(json!({ "status": "clicked", "selector": target }))
            }
            "type" => {
                if target.is_empty() { return Err("'type' requires 'target' selector".into()); }
                browser::type_text(&target, &text)?;
                Ok(json!({ "status": "typed", "selector": target, "length": text.len() }))
            }
            "read" => {
                if target.is_empty() { return Err("'read' requires 'target' selector".into()); }
                Ok(json!({ "selector": target, "text": browser::get_text(&target)? }))
            }
            "eval" => {
                if target.is_empty() { return Err("'eval' requires 'target' JS expression".into()); }
                Ok(json!({ "result": browser::evaluate_js(&target)? }))
            }
            "go_back" => {
                let wait_ms = args.get("wait").and_then(Value::as_u64).unwrap_or(0);
                browser::go_back(wait_ms)?;
                let url = browser::current_url().unwrap_or_default();
                Ok(json!({ "status": "went_back", "url": url }))
            }
            "wait" => {
                if target.is_empty() { return Err("'wait' requires 'target' selector".into()); }
                browser::wait_for_ms(&target, timeout.unwrap_or(5000))?;
                Ok(json!({ "status": "found", "selector": target }))
            }
            "exists" => {
                if target.is_empty() { return Err("'exists' requires 'target' selector".into()); }
                Ok(json!({ "selector": target, "exists": browser::element_exists(&target)? }))
            }
            "attr" => {
                if target.is_empty() { return Err("'attr' requires 'target' selector".into()); }
                if text.is_empty() { return Err("'attr' requires 'text' = attribute name".into()); }
                Ok(json!({ "selector": target, "attribute": &text, "value": browser::get_attribute(&target, &text)? }))
            }
            "scroll" => {
                let y = timeout.map(|t| t as f64).unwrap_or(300.0);
                browser::scroll_by(0.0, y)?;
                Ok(json!({ "status": "scrolled", "y": y }))
            }
            "key" => {
                if target.is_empty() { return Err("'key' requires 'target' key name".into()); }
                browser::press_key(&target)?;
                Ok(json!({ "status": "pressed", "key": target }))
            }
            "console" => {
                let limit = timeout.unwrap_or(20) as usize;
                let raw = browser::console_messages(limit).unwrap_or_else(|_| "[]".into());
                let val: Value = serde_json::from_str(&raw).unwrap_or(json!([]));
                Ok(json!({ "messages": val }))
            }
            "network" => {
                let limit = timeout.unwrap_or(20) as usize;
                let raw = browser::captured_requests(limit).unwrap_or_else(|_| "[]".into());
                let val: Value = serde_json::from_str(&raw).unwrap_or(json!([]));
                Ok(json!({ "requests": val }))
            }
            "url"   => Ok(json!({ "url": browser::current_url()? })),
            "title" => Ok(json!({ "title": browser::page_title()? })),
            "close" => { browser::close(); Ok(json!({ "status": "closed" })) }
            // Fixes #11: expose click_point + set_viewport so Flutter Web / CanvasKit
            // apps (no DOM) can be driven by coordinate rather than CSS selector.
            "click_point" => {
                let cx = x.ok_or("'click_point' requires 'x' (number)")?;
                let cy = y.ok_or("'click_point' requires 'y' (number)")?;
                // Issue #79: HiDPI screenshot coords need devicePixelRatio rescaling.
                let source = args.get("coord_source").and_then(Value::as_str).unwrap_or("css");
                match source {
                    "screenshot" => browser::click_point_screenshot(cx, cy)?,
                    _ => browser::click_point(cx, cy)?,
                }
                Ok(json!({ "status": "clicked", "x": cx, "y": cy, "coord_source": source }))
            }
            "set_viewport" => {
                let w = width.ok_or("'set_viewport' requires 'width' (positive integer)")? as u32;
                let h = height.ok_or("'set_viewport' requires 'height' (positive integer)")? as u32;
                browser::set_viewport(w, h, device_scale, mobile)?;
                Ok(json!({
                    "status": "viewport set",
                    "width": w, "height": h,
                    "device_scale": device_scale, "mobile": mobile
                }))
            }
            // ── Accessibility tree (literal text — for exact assertions) ──
            "enable_a11y" => {
                // Always call enable_flutter_semantics() to trigger the placeholder click
                // that builds flt-semantics-host DOM elements (used by shadow_find/shadow_dump).
                // Safety net: detect about:blank URL reset and restore.
                let saved_url = crate::browser::current_url().ok();
                let _ = crate::browser_ax::enable_flutter_semantics();
                // Poll until flt-semantics-host is non-empty (Flutter fills it async).
                let mut shadow_ready = false;
                for _ in 0..15 {
                    let count = browser::evaluate_js(
                        "document.querySelector('flt-semantics-host')?.childElementCount||0"
                    ).unwrap_or_default();
                    if count.trim() != "0" {
                        shadow_ready = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                if let Some(ref url) = saved_url {
                    let cur = crate::browser::current_url().unwrap_or_default();
                    if cur.contains("about:blank") && !url.contains("about:blank") && !url.is_empty() {
                        tracing::warn!(
                            "[browser_ax] enable_a11y: URL reset detected (about:blank) — restoring {url:?}"
                        );
                        let _ = crate::browser::navigate(url);
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
                let tree = crate::browser_ax::get_full_tree(false).unwrap_or_default();
                Ok(json!({ "status": "semantics enabled", "ax_node_count": tree.len(), "shadow_ready": shadow_ready }))
            }
            "ax_tree" => {
                let nodes = crate::browser_ax::get_full_tree(include_ignored)?;
                Ok(json!({ "count": nodes.len(), "nodes": nodes }))
            }
            "ax_find" => {
                if role_arg.is_none() && name_arg.is_none() {
                    return Err("'ax_find' requires 'role' and/or 'name'".into());
                }
                let name_regex_arg = args.get("name_regex").and_then(Value::as_str).map(String::from);
                let not_name_matches: Vec<String> = args.get("not_name_matches")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(1) as usize;

                // scroll=true: scroll-aware find (Issue #19 P1)
                if scroll_arg {
                    let (node, scrolled) = crate::browser_ax::find_scrolling_by_role_and_name(
                        role_arg.as_deref(), name_arg.as_deref(),
                        name_regex_arg.as_deref(), &not_name_matches,
                        scroll_max_arg, 400.0,
                    )?;
                    return Ok(json!({
                        "found": node.is_some(),
                        "node": node,
                        "scrolled_times": scrolled,
                    }));
                }

                if limit <= 1 {
                    match crate::browser_ax::find_by_role_and_name(
                        role_arg.as_deref(), name_arg.as_deref(),
                        name_regex_arg.as_deref(), &not_name_matches,
                    )? {
                        Some(node) => Ok(json!({ "found": true, "node": node })),
                        None => Ok(json!({ "found": false, "node": null })),
                    }
                } else {
                    let nodes = crate::browser_ax::find_all_by_role_and_name(
                        role_arg.as_deref(), name_arg.as_deref(),
                        name_regex_arg.as_deref(), &not_name_matches, limit,
                    )?;
                    Ok(json!({
                        "found": !nodes.is_empty(),
                        "count": nodes.len(),
                        "nodes": nodes,
                    }))
                }
            }
            "ax_snapshot" => {
                let snap_id_arg = args.get("id").and_then(Value::as_str).map(String::from);
                let id = crate::browser_ax::ax_snapshot(snap_id_arg.as_deref())?;
                Ok(json!({ "snapshot_id": id }))
            }
            "ax_diff" => {
                let before = args["before_id"].as_str().ok_or("'ax_diff' requires 'before_id'")?;
                let after  = args["after_id"].as_str().ok_or("'ax_diff' requires 'after_id'")?;
                let diff = crate::browser_ax::ax_diff(before, after)?;
                Ok(json!({
                    "added_count":   diff.added.len(),
                    "removed_count": diff.removed.len(),
                    "changed_count": diff.changed.len(),
                    "added":   diff.added.iter().map(|n| json!({"node_id": n.node_id, "role": n.role, "name": n.name})).collect::<Vec<_>>(),
                    "removed": diff.removed.iter().map(|n| json!({"node_id": n.node_id, "role": n.role, "name": n.name})).collect::<Vec<_>>(),
                    "changed": diff.changed,
                }))
            }
            "wait_for_ax_change" => {
                let baseline_id = args["baseline_id"].as_str().ok_or("'wait_for_ax_change' requires 'baseline_id'")?;
                let to_ms = timeout.unwrap_or(5000);
                let (new_id, diff) = crate::browser_ax::wait_for_ax_change(baseline_id, to_ms)?;
                Ok(json!({
                    "new_snapshot_id": new_id,
                    "added_count":   diff.added.len(),
                    "removed_count": diff.removed.len(),
                    "changed_count": diff.changed.len(),
                }))
            }
            "ax_value" => {
                let id = backend_id.ok_or("'ax_value' requires 'backend_id' (number)")?;
                Ok(json!({ "backend_id": id, "text": crate::browser_ax::read_node_text(id)? }))
            }
            "ax_click" => {
                let id = backend_id.ok_or("'ax_click' requires 'backend_id' (number)")?;
                crate::browser_ax::click_backend(id)?;
                Ok(json!({ "status": "clicked", "backend_id": id }))
            }
            "ax_focus" => {
                let id = backend_id.ok_or("'ax_focus' requires 'backend_id' (number)")?;
                crate::browser_ax::focus_backend(id)?;
                Ok(json!({ "status": "focused", "backend_id": id }))
            }
            "ax_type" => {
                let id = backend_id.ok_or("'ax_type' requires 'backend_id' (number)")?;
                crate::browser_ax::type_into_backend(id, &text)?;
                Ok(json!({ "status": "typed", "backend_id": id, "length": text.len() }))
            }
            "ax_type_verified" => {
                let id = backend_id.ok_or("'ax_type_verified' requires 'backend_id' (number)")?;
                let r = crate::browser_ax::type_into_backend_verified(id, &text)?;
                Ok(serde_json::to_value(&r).unwrap_or(json!({})))
            }
            // ── Test isolation ──────────────────────────────────────
            "clear_state" => {
                browser::clear_browser_state()?;
                Ok(json!({ "status": "cleared" }))
            }
            // ── Flutter Shadow DOM ──────────────────────────────────────────
            "shadow_find" => {
                let role = args.get("role").and_then(Value::as_str).map(String::from);
                let name = args.get("name_regex").or_else(|| args.get("name"))
                    .and_then(Value::as_str).map(String::from);
                let (x, y, label) = browser::shadow_find(role.as_deref(), name.as_deref())?;
                Ok(json!({ "found": true, "x": x, "y": y, "label": label }))
            }
            "shadow_click" => {
                let role = args.get("role").and_then(Value::as_str).map(String::from);
                let name = args.get("name_regex").or_else(|| args.get("name"))
                    .and_then(Value::as_str).map(String::from);
                let label = browser::shadow_click(role.as_deref(), name.as_deref())?;
                Ok(json!({ "status": "clicked", "label": label }))
            }
            "shadow_type" => {
                let role = args.get("role").and_then(Value::as_str).map(String::from);
                let name = args.get("name_regex").or_else(|| args.get("name"))
                    .and_then(Value::as_str).map(String::from);
                let text_val = args.get("text").and_then(Value::as_str)
                    .ok_or("'shadow_type' requires 'text'")?;
                browser::shadow_type(role.as_deref(), name.as_deref(), text_val)?;
                Ok(json!({ "status": "typed", "text": text_val }))
            }
            "flutter_type" => {
                // Accept both string "50" and number 50 (sirin-call key=value parses ints as JSON numbers)
                let text_owned = args.get("text")
                    .map(|v| if let Some(s) = v.as_str() { s.to_string() } else { v.to_string().trim_matches('"').to_string() })
                    .ok_or("'flutter_type' requires 'text'")?;
                browser::flutter_type(&text_owned)?;
                Ok(json!({ "status": "typed", "text": text_owned }))
            }
            "flutter_enter" => {
                // Send Enter key to flt-text-editing — submits chat message or form
                let result = browser::flutter_enter()?;
                Ok(json!({ "status": "ok", "result": result }))
            }
            "shadow_type_flutter" => {
                let role = args.get("role").and_then(Value::as_str).map(String::from);
                let name = args.get("name_regex").or_else(|| args.get("name"))
                    .and_then(Value::as_str).map(String::from);
                let text_owned = args.get("text")
                    .map(|v| if let Some(s) = v.as_str() { s.to_string() } else { v.to_string().trim_matches('"').to_string() })
                    .ok_or("'shadow_type_flutter' requires 'text'")?;
                let label = browser::shadow_click(role.as_deref(), name.as_deref())?;
                std::thread::sleep(std::time::Duration::from_millis(350));
                browser::flutter_type(&text_owned)?;
                Ok(json!({ "status": "typed", "label": label, "text": text_owned }))
            }
            "shadow_dump" => {
                let items = browser::shadow_dump()?;
                Ok(json!({ "count": items.len(), "elements": items }))
            }
            // ── Multi-tab / popup ───────────────────────────────────
            "wait_new_tab" => {
                let to_ms = timeout.unwrap_or(10000);
                // baseline=None → fn measures from same source as its loop
                let idx = browser::wait_for_new_tab(None, to_ms)?;
                Ok(json!({ "status": "new tab opened", "active_tab": idx }))
            }
            // ── Network ─────────────────────────────────────────────
            "wait_request" => {
                if target.is_empty() {
                    return Err("'wait_request' requires 'target' = URL substring".into());
                }
                let to_ms = timeout.unwrap_or(10000);
                let raw = browser::wait_for_request(&target, to_ms)?;
                let val: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
                Ok(json!({ "request": val }))
            }
            // ── Condition-based waits (Issue #19 P0) ─────────────────
            "wait_for_url" => {
                if target.is_empty() {
                    return Err("'wait_for_url' requires 'target' (URL substring or /regex/)".into());
                }
                let to_ms = timeout.unwrap_or(10000);
                let elapsed = browser::wait_for_url(&target, to_ms)?;
                let url = browser::current_url().unwrap_or_default();
                Ok(json!({ "status": "ready", "elapsed_ms": elapsed, "url": url }))
            }
            "wait_for_ax_ready" => {
                let to_ms = timeout.unwrap_or(10000);
                let (elapsed, count) = crate::browser_ax::wait_for_ax_ready(min_nodes_arg, to_ms)?;
                Ok(json!({ "status": "ready", "elapsed_ms": elapsed, "ax_node_count": count }))
            }
            "wait_for_network_idle" => {
                let to_ms = timeout.unwrap_or(15000);
                let elapsed = browser::wait_for_network_idle(idle_ms_arg, to_ms)?;
                Ok(json!({ "status": "idle", "elapsed_ms": elapsed }))
            }
            // ── Assertions (Issue #19 bonus) ──────────────────────────
            "assert_ax_contains" => {
                if target.is_empty() {
                    return Err("'assert_ax_contains' requires 'target' = text to find".into());
                }
                let tree = crate::browser_ax::get_full_tree(false)?;
                let needle = target.to_lowercase();
                let found = tree.iter().any(|n| {
                    n.name.as_deref().unwrap_or("").to_lowercase().contains(&needle)
                        || n.value.as_deref().unwrap_or("").to_lowercase().contains(&needle)
                });
                let preview: Vec<String> = tree.iter().take(20)
                    .filter_map(|n| n.name.clone().or_else(|| n.value.clone()))
                    .collect();
                Ok(json!({
                    "passed": found,
                    "target": target,
                    "actual_ax_tree_preview": preview.join(" | "),
                }))
            }
            "assert_url_matches" => {
                if target.is_empty() {
                    return Err("'assert_url_matches' requires 'target' (URL substring or /regex/)".into());
                }
                let url = browser::current_url().unwrap_or_default();
                let is_regex = target.starts_with('/') && target.ends_with('/') && target.len() > 2;
                let passed = if is_regex {
                    let pattern = &target[1..target.len() - 1];
                    regex::Regex::new(pattern).map(|re| re.is_match(&url)).unwrap_or(false)
                } else {
                    url.contains(&target)
                };
                Ok(json!({ "passed": passed, "target": target, "actual_url": url }))
            }
            // ── Named sessions (Issue #19 P1) ─────────────────────────
            "list_sessions" => {
                let sessions = browser::list_sessions().unwrap_or_default();
                let items: Vec<Value> = sessions.into_iter().map(|(id, idx, url)| {
                    json!({ "session_id": id, "tab_index": idx, "url": url })
                }).collect();
                Ok(json!({ "count": items.len(), "sessions": items }))
            }
            "close_session" => {
                if target.is_empty() {
                    return Err("'close_session' requires 'target' = session_id".into());
                }
                browser::close_session(&target)?;
                Ok(json!({ "status": "closed", "session_id": target }))
            }
            // ── Sirin Companion extension probes (POC) ───────────────────────
            "ext_status" => {
                Ok(serde_json::to_value(crate::ext_server::status())
                    .map_err(|e| format!("ext_status serialize: {e}"))?)
            }
            "ext_url" => {
                // Authoritative URL from extension; falls back to CDP cache.
                let tab_id = args.get("tab_id").and_then(Value::as_i64);
                match crate::ext_server::authoritative_url(tab_id) {
                    Some(u) => Ok(json!({ "url": u, "source": "extension" })),
                    None    => Ok(json!({
                        "url":    browser::current_url().unwrap_or_default(),
                        "source": "cdp_cache_fallback",
                    })),
                }
            }
            "ext_tabs" => {
                Ok(json!({ "tabs": crate::ext_server::list_tabs() }))
            }
            other   => Err(format!("Unknown browser_exec action: {other}")),
        }
    })
    .await;

    let dur = t0.elapsed().as_millis() as u64;
    match &result {
        Ok(v)  => crate::monitor::emit_action_done(&action_id, v.clone(), dur).await,
        Err(e) => crate::monitor::emit_action_error(&action_id, e).await,
    }
    result
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
