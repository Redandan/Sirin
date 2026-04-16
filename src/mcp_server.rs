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
    response::IntoResponse,
    routing::post,
    Router,
};
use serde_json::{json, Value};

// ── Router ────────────────────────────────────────────────────────────────────

pub fn mcp_router() -> Router {
    Router::new().route("/mcp", post(mcp_handler))
}

// ── Handler ───────────────────────────────────────────────────────────────────

async fn mcp_handler(Json(req): Json<Value>) -> impl IntoResponse {
    let id     = req.get("id").cloned().unwrap_or(json!(null));
    let method = req["method"].as_str().unwrap_or("").to_string();
    let params = req.get("params").cloned().unwrap_or(json!({}));

    let result = dispatch(&method, params).await;

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

async fn dispatch(method: &str, params: Value) -> Result<Value, String> {
    match method {
        "initialize" => handle_initialize(params),
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tools_call(params).await,
        // Notifications (no response required, but we must not error)
        "notifications/initialized" => Ok(json!({})),
        other => Err(format!("Method not found: {other}")),
    }
}

// ── initialize ────────────────────────────────────────────────────────────────

fn handle_initialize(_params: Value) -> Result<Value, String> {
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
                        "locale":         { "type": "string", "description": "zh-TW / en / zh-CN（預設 zh-TW）" },
                        "max_iterations": { "type": "number", "description": "預設 15" },
                        "timeout_secs":   { "type": "number", "description": "預設 120" }
                    },
                    "required": ["url", "goal"]
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
                "name": "browser_exec",
                "description": "即席執行瀏覽器動作，不走完整 test goal。適合 debug / 探索 / 單步操作。action 可用：goto, screenshot, click, type, read, eval, wait, exists, attr, scroll, key, console, network, url, title, close。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action":  { "type": "string", "description": "web_navigate action 名稱" },
                        "target":  { "type": "string", "description": "URL / CSS selector / JS expression，視 action 而定" },
                        "text":    { "type": "string", "description": "type 動作的輸入文字" },
                        "timeout": { "type": "number", "description": "wait 動作的 ms" }
                    },
                    "required": ["action"]
                }
            }
        ]
    }))
}

// ── tools/call ────────────────────────────────────────────────────────────────

async fn handle_tools_call(params: Value) -> Result<Value, String> {
    let name      = params["name"].as_str().ok_or("Missing 'name'")?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    // Tools that return structured JSON (not just text) bypass the text wrapper.
    match name {
        "list_tests"           => return call_list_tests(arguments).map(wrap_json),
        "run_test_async"       => return call_run_test_async(arguments).map(wrap_json),
        "run_adhoc_test"       => return call_run_adhoc_test(arguments).map(wrap_json),
        "get_test_result"      => return call_get_test_result(arguments).map(wrap_json),
        "get_screenshot"       => return call_get_screenshot(arguments).map(wrap_json),
        "get_full_observation" => return call_get_full_observation(arguments).map(wrap_json),
        "list_recent_runs"     => return call_list_recent_runs(arguments).map(wrap_json),
        "list_fixes"           => return call_list_fixes(arguments).map(wrap_json),
        "config_diagnostics"   => return call_config_diagnostics().map(wrap_json),
        "browser_exec"         => return call_browser_exec(arguments).await.map(wrap_json),
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

    tokio::task::spawn_blocking(move || {
        crate::memory::memory_search(&query, limit, "")
            .map(|results| results.join("\n\n"))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))?
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
        }))
        .collect();
    Ok(json!({ "count": items.len(), "tests": items }))
}

fn call_run_test_async(args: Value) -> Result<Value, String> {
    let test_id = args["test_id"].as_str().ok_or("Missing test_id")?.to_string();
    let auto_fix = args.get("auto_fix").and_then(Value::as_bool).unwrap_or(false);
    let run_id = crate::test_runner::spawn_run_async(test_id.clone(), auto_fix)?;
    Ok(json!({
        "run_id": run_id,
        "test_id": test_id,
        "auto_fix": auto_fix,
        "status": "queued",
        "poll_with": "get_test_result",
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

    let run_id = crate::test_runner::spawn_adhoc_run(
        url.clone(), goal, criteria, locale, max_iter, timeout,
    )?;
    Ok(json!({
        "run_id": run_id,
        "url": url,
        "status": "queued",
        "poll_with": "get_test_result",
    }))
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

async fn call_browser_exec(args: Value) -> Result<Value, String> {
    let action = args["action"].as_str().ok_or("Missing action")?.to_string();
    let target = args.get("target").and_then(Value::as_str).unwrap_or("").to_string();
    let text   = args.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    let timeout = args.get("timeout").and_then(Value::as_u64);

    // Dispatch directly to crate::browser to avoid requiring an AgentContext
    // for simple imperative calls.  Mirrors the web_navigate action set.
    tokio::task::spawn_blocking(move || -> Result<Value, String> {
        use crate::browser;
        match action.as_str() {
            "goto" => {
                if target.is_empty() { return Err("'goto' requires 'target' URL".into()); }
                browser::ensure_open(true)?;
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
            other   => Err(format!("Unknown browser_exec action: {other}")),
        }
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))?
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
        let r = rt.block_on(call_browser_exec(json!({})));
        assert!(r.is_err());
    }
}

/// Minimal base64 encoder (no external dep).
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
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
