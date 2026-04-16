//! App-level state — logs, task tracker, system status, memory search,
//! persona/LLM/config editing, and toast event buffer.

use super::RealService;
use crate::ui_service::*;

// ── Tasks ────────────────────────────────────────────────────────────────────

pub(super) fn recent_tasks(svc: &RealService, limit: usize) -> Vec<TaskView> {
    svc.tracker.read_last_n(limit).unwrap_or_default()
        .into_iter()
        .filter(|e| e.event != "heartbeat")
        .rev()
        .map(|e| TaskView { timestamp: e.timestamp, event: e.event, status: e.status, reason: e.reason })
        .collect()
}

// ── Log ──────────────────────────────────────────────────────────────────────

pub(super) fn log_version(_svc: &RealService) -> usize { crate::log_buffer::version() }

pub(super) fn log_recent(_svc: &RealService, limit: usize) -> Vec<LogLine> {
    crate::log_buffer::recent(limit).into_iter()
        .map(|text| { let level = classify_log_level(&text); LogLine { text, level } })
        .collect()
}

pub(super) fn log_len(_svc: &RealService) -> usize { crate::log_buffer::len() }

pub(super) fn log_clear(_svc: &RealService) { crate::log_buffer::clear(); }

fn classify_log_level(line: &str) -> LogLevel {
    let lower = line.to_lowercase();
    if line.contains("[ERROR]") || lower.contains("error") || lower.contains("failed") { LogLevel::Error }
    else if line.contains("[WARN]") || lower.contains("warn") { LogLevel::Warn }
    else if line.contains("[telegram]") || line.contains("[tg]") { LogLevel::Telegram }
    else if line.contains("[researcher]") { LogLevel::Research }
    else if line.contains("[followup]") { LogLevel::Followup }
    else if line.contains("[coding]") || line.contains("[adk]") { LogLevel::Coding }
    else if line.contains("[teams]") { LogLevel::Teams }
    else { LogLevel::Normal }
}

// ── System status + memory search ────────────────────────────────────────────

pub(super) fn system_status(svc: &RealService) -> SystemStatus {
    let tg = svc.tg_auth.status();
    let tg_connected = matches!(tg, crate::telegram_auth::TelegramStatus::Connected);
    let llm = crate::llm::shared_llm();
    let router = crate::llm::shared_router_llm();
    SystemStatus {
        telegram_connected: tg_connected,
        telegram_status: format!("{:?}", tg),
        rpc_running: crate::rpc_server::is_running(),
        llm: LlmInfo {
            main_model: llm.model.clone(), main_backend: llm.backend_name().to_string(),
            router_model: router.model.clone(), router_backend: router.backend_name().to_string(),
            is_remote: llm.is_remote(),
        },
        mcp_tools: crate::mcp_client::get_discovered_tools().iter()
            .map(|t| McpToolInfo { name: t.registry_name(), description: t.description.clone() }).collect(),
        skills: crate::skills::list_skills().iter()
            .map(|s| SkillInfo { name: s.name.clone(), category: s.category.clone(), description: s.description.clone() }).collect(),
    }
}

pub(super) fn search_memory(_svc: &RealService, query: &str, limit: usize) -> Vec<String> {
    crate::memory::memory_search(query, limit, "").unwrap_or_default()
}

// ── Persona editing ──────────────────────────────────────────────────────────

pub(super) fn persona_name(_svc: &RealService) -> String {
    crate::persona::Persona::cached().map(|p| p.name().to_string()).unwrap_or_default()
}

pub(super) fn set_persona_name(_svc: &RealService, name: &str) {
    if let Ok(mut p) = crate::persona::Persona::load() {
        p.identity.name = name.to_string();
        if let Ok(yaml) = serde_yaml::to_string(&p) {
            let _ = std::fs::write("config/persona.yaml", yaml);
            crate::persona::Persona::reload_cache();
        }
    }
}

pub(super) fn persona_objectives(_svc: &RealService) -> Vec<String> {
    crate::persona::Persona::cached().map(|p| p.objectives.clone()).unwrap_or_default()
}

pub(super) fn set_persona_objectives(_svc: &RealService, objectives: Vec<String>) {
    if let Ok(mut p) = crate::persona::Persona::load() {
        p.objectives = objectives;
        if let Ok(yaml) = serde_yaml::to_string(&p) {
            let _ = std::fs::write("config/persona.yaml", yaml);
            crate::persona::Persona::reload_cache();
        }
    }
}

pub(super) fn persona_voice(_svc: &RealService) -> String {
    crate::persona::Persona::cached()
        .map(|p| p.response_style.voice.clone())
        .unwrap_or_default()
}

pub(super) fn set_persona_voice(_svc: &RealService, voice: &str) {
    if let Ok(mut p) = crate::persona::Persona::load() {
        p.response_style.voice = voice.to_string();
        if let Ok(yaml) = serde_yaml::to_string(&p) {
            let _ = std::fs::write("config/persona.yaml", yaml);
            crate::persona::Persona::reload_cache();
        }
    }
}

// ── LLM model selection ──────────────────────────────────────────────────────

pub(super) fn available_models(_svc: &RealService) -> Vec<String> {
    let fleet = crate::llm::shared_fleet();
    fleet.classified_models.iter().map(|m| m.info.name.clone()).collect()
}

pub(super) fn set_main_model(svc: &RealService, model: &str) {
    let mut cfg = crate::llm::LlmUiConfig::load();
    cfg.main_model = model.to_string();
    let _ = cfg.save();
    svc.push_toast(ToastLevel::Info, format!("模型已切換為 {model}（重啟生效）"));
}

// ── Config export/import ─────────────────────────────────────────────────────

pub(super) fn export_config(_svc: &RealService) -> String {
    std::fs::read_to_string("config/agents.yaml").unwrap_or_else(|_| "# empty".to_string())
}

pub(super) fn import_config(svc: &RealService, yaml: &str) -> Result<(), String> {
    let _: crate::agent_config::AgentsFile = serde_yaml::from_str(yaml)
        .map_err(|e| format!("YAML 解析失敗: {e}"))?;
    std::fs::write("config/agents.yaml", yaml).map_err(|e| format!("寫入失敗: {e}"))?;
    svc.push_toast(ToastLevel::Success, "設定已匯入");
    Ok(())
}

// ── Toast events ─────────────────────────────────────────────────────────────

pub(super) fn poll_toasts(svc: &RealService) -> Vec<ToastEvent> {
    svc.drain_toasts()
}

pub(super) fn toast_history(svc: &RealService) -> Vec<ToastEvent> {
    svc.toast_history_snapshot()
}

// ── Config check ─────────────────────────────────────────────────────────────

pub(super) fn config_check(_svc: &RealService) -> Vec<ConfigIssueView> {
    crate::config_check::run_diagnostics()
        .into_iter()
        .map(|i| ConfigIssueView {
            severity: match i.severity {
                crate::config_check::Severity::Ok => ConfigSeverity::Ok,
                crate::config_check::Severity::Info => ConfigSeverity::Info,
                crate::config_check::Severity::Warning => ConfigSeverity::Warning,
                crate::config_check::Severity::Error => ConfigSeverity::Error,
            },
            category: i.category.to_string(),
            message: i.message,
            suggestion: i.suggestion,
        })
        .collect()
}

pub(super) fn config_ai_analyze(_svc: &RealService) -> Result<AiAdviceView, String> {
    // LLM call is async — bridge to sync via tokio runtime
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    let advice = rt.block_on(crate::config_check::ai_analyze())?;
    Ok(AiAdviceView {
        analysis: advice.analysis,
        proposed_fixes: advice.proposed_fixes.into_iter().map(|f| ConfigFixView {
            file: f.file,
            field_path: f.field_path,
            current_value: f.current_value,
            new_value: f.new_value,
            reason: f.reason,
        }).collect(),
    })
}

pub(super) fn config_apply_fixes(svc: &RealService, fixes: Vec<ConfigFixView>) -> Result<Vec<String>, String> {
    let core_fixes: Vec<crate::config_check::ConfigFix> = fixes.into_iter().map(|f| crate::config_check::ConfigFix {
        file: f.file,
        field_path: f.field_path,
        current_value: f.current_value,
        new_value: f.new_value,
        reason: f.reason,
    }).collect();
    let applied = crate::config_check::apply_fixes(&core_fixes)?;
    svc.push_toast(ToastLevel::Success, format!("已套用 {} 項配置修改（重啟生效）", applied.len()));
    Ok(applied)
}
