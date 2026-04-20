//! Agent CRUD, objectives, behavior, and per-agent skill toggles.

use super::RealService;
use crate::ui_service::*;

/// `AgentsFile::load()` reads + YAML-parses `agents.yaml` from disk on every
/// call. The UI hot path (workspace + sidebar) hits it 3+ times per frame
/// (`list_agents`, `agent_detail`, `disabled_skills`), so mouse hover alone
/// can drive 50+ disk reads/sec on a typical machine — that's the perceived
/// "view-switch lag". 1 s TTL: cheap to refresh, still feels live.
///
/// Mutation paths (rename / toggle / add objective / etc.) call
/// `invalidate_agents_cache()` after `save()` so writes show up immediately.
fn agents_file_cache()
-> &'static std::sync::Mutex<(std::time::Instant, Option<crate::agent_config::AgentsFile>)> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<(std::time::Instant, Option<crate::agent_config::AgentsFile>)>
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(
        (std::time::Instant::now() - std::time::Duration::from_secs(60), None)
    ))
}

fn cached_agents_file() -> crate::agent_config::AgentsFile {
    const TTL: std::time::Duration = std::time::Duration::from_secs(1);
    {
        let g = agents_file_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = &g.1 {
            if g.0.elapsed() < TTL { return v.clone(); }
        }
    }
    let v = crate::agent_config::AgentsFile::load().unwrap_or_default();
    let mut g = agents_file_cache().lock().unwrap_or_else(|e| e.into_inner());
    *g = (std::time::Instant::now(), Some(v.clone()));
    v
}

/// Drop the cache so the next read re-parses agents.yaml. Call after save().
fn invalidate_agents_cache() {
    let mut g = agents_file_cache().lock().unwrap_or_else(|e| e.into_inner());
    *g = (std::time::Instant::now() - std::time::Duration::from_secs(60), None);
}

pub(super) fn list_agents(svc: &RealService) -> Vec<AgentSummary> {
    let file = cached_agents_file();
    file.agents.iter().map(|a| {
        let platform = if a.channel.as_ref().and_then(|c| c.telegram.as_ref()).is_some() {
            "telegram"
        } else if a.channel.as_ref().and_then(|c| c.teams.as_ref()).is_some() {
            "teams"
        } else { "ui_only" };
        let live_status = if !a.enabled { "idle" }
            else if platform == "telegram" {
                match svc.tg_auth.status() {
                    crate::telegram_auth::TelegramStatus::Connected => "connected",
                    crate::telegram_auth::TelegramStatus::Disconnected { .. } => "reconnecting",
                    _ => "waiting",
                }
            } else { "idle" };
        AgentSummary { id: a.id.clone(), name: a.identity.name.clone(), enabled: a.enabled,
            platform: platform.to_string(), live_status: live_status.to_string() }
    }).collect()
}

pub(super) fn agent_detail(_svc: &RealService, agent_id: &str) -> Option<AgentDetailView> {
    let file = cached_agents_file();
    let a = file.agents.iter().find(|a| a.id == agent_id)?;
    let platform = if a.channel.as_ref().and_then(|c| c.telegram.as_ref()).is_some() { "telegram" }
    else if a.channel.as_ref().and_then(|c| c.teams.as_ref()).is_some() { "teams" }
    else { "ui_only" };
    Some(AgentDetailView {
        id: a.id.clone(), name: a.identity.name.clone(), enabled: a.enabled,
        platform: platform.to_string(),
        professional_tone: format!("{:?}", a.identity.professional_tone),
        disable_remote_ai: a.disable_remote_ai,
        objectives: a.objectives.clone(),
        human_behavior_enabled: a.human_behavior.enabled,
        min_reply_delay: a.human_behavior.min_reply_delay_secs,
        max_reply_delay: a.human_behavior.max_reply_delay_secs,
        max_per_hour: a.human_behavior.max_messages_per_hour,
        max_per_day: a.human_behavior.max_messages_per_day,
        kpi_labels: a.kpi.metrics.iter().map(|m| (m.label.clone(), m.unit.clone())).collect(),
    })
}

pub(super) fn create_agent(svc: &RealService, id: &str, name: &str) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        let agent = crate::agent_config::AgentConfig::new_default(id, name);
        file.agents.push(agent);
        let _ = file.save();
        invalidate_agents_cache();
        svc.push_toast(ToastLevel::Success, format!("Agent「{name}」已建立"));
    }
}

pub(super) fn rename_agent(svc: &RealService, agent_id: &str, new_name: &str) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.identity.name = new_name.to_string();
            let _ = file.save();
            invalidate_agents_cache();
            svc.push_toast(ToastLevel::Success, format!("已改名為「{new_name}」"));
        }
    }
}

pub(super) fn toggle_agent(_svc: &RealService, agent_id: &str, enabled: bool) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.enabled = enabled;
            let _ = file.save();
            invalidate_agents_cache();
        }
    }
}

pub(super) fn delete_agent(svc: &RealService, agent_id: &str) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        file.agents.retain(|a| a.id != agent_id);
        let _ = file.save();
        invalidate_agents_cache();
        svc.push_toast(ToastLevel::Info, format!("Agent {agent_id} 已刪除"));
    }
}

pub(super) fn add_objective(_svc: &RealService, agent_id: &str, text: &str) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.objectives.push(text.to_string());
            let _ = file.save();
            invalidate_agents_cache();
        }
    }
}

pub(super) fn remove_objective(_svc: &RealService, agent_id: &str, index: usize) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            if index < a.objectives.len() {
                a.objectives.remove(index);
                let _ = file.save();
                invalidate_agents_cache();
            }
        }
    }
}

pub(super) fn set_remote_ai(_svc: &RealService, agent_id: &str, allowed: bool) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.disable_remote_ai = !allowed;
            let _ = file.save();
            invalidate_agents_cache();
        }
    }
}

pub(super) fn set_behavior(
    _svc: &RealService,
    agent_id: &str,
    enabled: bool,
    min_delay: u64,
    max_delay: u64,
    max_hour: u32,
    max_day: u32,
) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.human_behavior.enabled = enabled;
            a.human_behavior.min_reply_delay_secs = min_delay;
            a.human_behavior.max_reply_delay_secs = max_delay;
            a.human_behavior.max_messages_per_hour = max_hour;
            a.human_behavior.max_messages_per_day = max_day;
            let _ = file.save();
            invalidate_agents_cache();
        }
    }
}

pub(super) fn toggle_skill(_svc: &RealService, agent_id: &str, skill_id: &str, enabled: bool) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            if enabled {
                a.disabled_skills.retain(|s| s != skill_id);
            } else if !a.disabled_skills.contains(&skill_id.to_string()) {
                a.disabled_skills.push(skill_id.to_string());
            }
            let _ = file.save();
            invalidate_agents_cache();
        }
    }
}

pub(super) fn disabled_skills(_svc: &RealService, agent_id: &str) -> Vec<String> {
    cached_agents_file()
        .agents.iter().find(|a| a.id == agent_id)
        .map(|a| a.disabled_skills.clone())
        .unwrap_or_default()
}
