//! External integrations — Telegram auth, Teams, MCP, Meeting, Chat/Research
//! triggers, and local skill execution.

use super::RealService;
use crate::ui_service::*;

// ── Telegram auth ────────────────────────────────────────────────────────────

pub(super) fn tg_submit_code(svc: &RealService, code: &str) -> bool {
    svc.tg_auth.submit_code(code.to_string())
}

pub(super) fn tg_submit_password(svc: &RealService, password: &str) -> bool {
    svc.tg_auth.submit_password(password.to_string())
}

pub(super) fn tg_reconnect(svc: &RealService) {
    svc.tg_auth.trigger_reconnect();
}

// ── Teams ────────────────────────────────────────────────────────────────────

pub(super) fn start_teams(svc: &RealService) {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async { crate::teams::run_poller().await });
        svc.push_toast(ToastLevel::Info, "Teams 連線啟動中...");
    }
}

pub(super) fn teams_running(_svc: &RealService) -> bool {
    !matches!(crate::teams::session_status(), crate::teams::SessionStatus::NotStarted)
}

// ── MCP ──────────────────────────────────────────────────────────────────────

pub(super) fn mcp_tools(_svc: &RealService) -> Vec<McpToolDetail> {
    // Settings panel calls this on every UI repaint. Each call clones every
    // discovered tool's full schema (description + params). Cache 5 s — MCP
    // tool list only changes when servers reconnect.
    const TTL: std::time::Duration = std::time::Duration::from_secs(5);
    static CACHE: std::sync::OnceLock<std::sync::Mutex<(std::time::Instant, Vec<McpToolDetail>)>> =
        std::sync::OnceLock::new();
    let cell = CACHE.get_or_init(|| std::sync::Mutex::new(
        (std::time::Instant::now() - std::time::Duration::from_secs(60), Vec::new())
    ));
    {
        let g = cell.lock().unwrap_or_else(|e| e.into_inner());
        if !g.1.is_empty() && g.0.elapsed() < TTL { return g.1.clone(); }
    }

    let v: Vec<McpToolDetail> = crate::mcp_client::get_discovered_tools().iter().map(|t| {
        let params = t.input_schema.get("properties")
            .and_then(|p| p.as_object())
            .map(|props| {
                props.iter().map(|(k, v)| {
                    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("string");
                    (k.clone(), typ.to_string())
                }).collect()
            })
            .unwrap_or_default();
        McpToolDetail {
            server_name: t.server_name.clone(),
            tool_name: t.tool_name.clone(),
            registry_name: t.registry_name(),
            description: t.description.clone(),
            params,
        }
    }).collect();

    let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
    *g = (std::time::Instant::now(), v.clone());
    v
}

pub(super) fn mcp_call(_svc: &RealService, tool_name: &str, args_json: &str) -> Result<String, String> {
    let tools = crate::mcp_client::get_discovered_tools();
    let tool = tools.iter().find(|t| t.registry_name() == tool_name || t.tool_name == tool_name)
        .ok_or_else(|| format!("Tool not found: {tool_name}"))?;

    let args: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| format!("Invalid JSON: {e}"))?;

    let url = tool.server_url.clone();
    let name = tool.tool_name.clone();

    // Run synchronously (UI blocks briefly — acceptable for tool calls).
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "No tokio runtime".to_string())?;
    let result = std::thread::spawn(move || {
        rt.block_on(crate::mcp_client::call_tool(&url, &name, args))
    }).join().map_err(|_| "Thread panic".to_string())??;

    Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| format!("{result}")))
}

/// Call a Sirin-local MCP tool at http://127.0.0.1:7700/mcp.
pub(super) fn sirin_mcp_call(_svc: &crate::ui_service_impl::RealService, tool_name: &str, args_json: &str) -> Result<String, String> {
    let url   = "http://127.0.0.1:7700/mcp".to_string();
    let name  = tool_name.to_string();
    let args: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| format!("Invalid JSON: {e}"))?;

    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "No tokio runtime".to_string())?;
    let result = std::thread::spawn(move || {
        rt.block_on(crate::mcp_client::call_tool(&url, &name, args))
    }).join().map_err(|_| "Thread panic".to_string())??;

    Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| format!("{result}")))
}

// ── Meeting ──────────────────────────────────────────────────────────────────

pub(super) fn meeting_active(_svc: &RealService) -> bool {
    crate::meeting::current_meeting_id().is_some()
}

pub(super) fn meeting_start(svc: &RealService, participants: Vec<String>) -> String {
    let id = crate::meeting::start_meeting(participants);
    svc.push_toast(ToastLevel::Success, "會議已開始");
    id
}

pub(super) fn meeting_end(svc: &RealService) {
    crate::meeting::end_meeting();
    svc.push_toast(ToastLevel::Info, "會議已結束");
}

pub(super) fn meeting_send(_svc: &RealService, speaker: &str, text: &str) {
    crate::meeting::append_turn(speaker, text);
}

pub(super) fn meeting_history(_svc: &RealService) -> Vec<(String, String)> {
    crate::meeting::get_turns()
}

// ── Chat ─────────────────────────────────────────────────────────────────────

pub(super) fn chat_send(svc: &RealService, _agent_id: &str, message: &str) -> String {
    use crate::agents::chat_agent::{run_chat_via_adk_with_tracker, ChatRequest};

    let request = ChatRequest {
        user_text: message.to_string(),
        ..Default::default()
    };

    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        let tracker = svc.tracker.clone();
        std::thread::spawn(move || {
            rt.block_on(run_chat_via_adk_with_tracker(request, Some(tracker)))
        }).join().unwrap_or_else(|_| "（Agent 回覆失敗）".to_string())
    } else {
        "（無法取得 Tokio runtime）".to_string()
    }
}

// ── Research trigger ─────────────────────────────────────────────────────────

pub(super) fn trigger_research(svc: &RealService, topic: &str, url: Option<&str>) {
    crate::events::publish(crate::events::AgentEvent::ResearchRequested {
        topic: topic.to_string(),
        url: url.map(|s| s.to_string()),
    });
    svc.push_toast(ToastLevel::Info, format!("已觸發調研：{topic}"));
}

// ── Skill execution ──────────────────────────────────────────────────────────

pub(super) fn execute_skill(_svc: &RealService, skill_id: &str, input: &str) -> String {
    let script_path = crate::platform::config_dir().join("scripts").join(format!("{skill_id}.rhai"));
    if script_path.exists() {
        return crate::rhai_engine::run_rhai_script(script_path.to_str().unwrap_or(""), skill_id, input, None)
            .unwrap_or_else(|e| format!("錯誤: {e}"));
    }
    format!("技能 {skill_id} 沒有可執行的腳本")
}
