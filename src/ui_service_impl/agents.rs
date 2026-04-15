//! Agent CRUD, objectives, behavior, and per-agent skill toggles.

use super::RealService;
use crate::ui_service::*;

pub(super) fn list_agents(svc: &RealService) -> Vec<AgentSummary> {
    let file = crate::agent_config::AgentsFile::load().unwrap_or_default();
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
    let file = crate::agent_config::AgentsFile::load().unwrap_or_default();
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
        svc.push_toast(ToastLevel::Success, format!("Agent「{name}」已建立"));
    }
}

pub(super) fn rename_agent(svc: &RealService, agent_id: &str, new_name: &str) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.identity.name = new_name.to_string();
            let _ = file.save();
            svc.push_toast(ToastLevel::Success, format!("已改名為「{new_name}」"));
        }
    }
}

pub(super) fn toggle_agent(_svc: &RealService, agent_id: &str, enabled: bool) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.enabled = enabled;
            let _ = file.save();
        }
    }
}

pub(super) fn delete_agent(svc: &RealService, agent_id: &str) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        file.agents.retain(|a| a.id != agent_id);
        let _ = file.save();
        svc.push_toast(ToastLevel::Info, format!("Agent {agent_id} 已刪除"));
    }
}

pub(super) fn add_objective(_svc: &RealService, agent_id: &str, text: &str) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.objectives.push(text.to_string());
            let _ = file.save();
        }
    }
}

pub(super) fn remove_objective(_svc: &RealService, agent_id: &str, index: usize) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            if index < a.objectives.len() {
                a.objectives.remove(index);
                let _ = file.save();
            }
        }
    }
}

pub(super) fn set_remote_ai(_svc: &RealService, agent_id: &str, allowed: bool) {
    if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
        if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
            a.disable_remote_ai = !allowed;
            let _ = file.save();
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
        }
    }
}

pub(super) fn disabled_skills(_svc: &RealService, agent_id: &str) -> Vec<String> {
    crate::agent_config::AgentsFile::load().ok()
        .and_then(|f| f.agents.iter().find(|a| a.id == agent_id).map(|a| a.disabled_skills.clone()))
        .unwrap_or_default()
}
