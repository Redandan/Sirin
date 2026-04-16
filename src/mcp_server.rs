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
//! | `get_test_result` | 依 run_id 取得測試狀態或結果 |
//! | `get_screenshot` | 依 run_id 取得截圖（base64 PNG）|
//! | `get_full_observation` | 取得某步驟的完整（未截斷）observation |

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
        "get_test_result"      => return call_get_test_result(arguments).map(wrap_json),
        "get_screenshot"       => return call_get_screenshot(arguments).map(wrap_json),
        "get_full_observation" => return call_get_full_observation(arguments).map(wrap_json),
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
