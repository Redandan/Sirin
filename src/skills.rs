use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub requires_approval: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExecutionResult {
    pub skill_id: String,
    pub emitted_event: String,
    pub accepted: bool,
}

pub fn list_skills() -> Vec<SkillDefinition> {
    vec![SkillDefinition {
        id: "send_tg_reply".to_string(),
        name: "Send Telegram Reply".to_string(),
        description: "Emits a skill event for the Telegram module to send a reply.".to_string(),
        requires_approval: true,
    }]
}

pub fn ensure_registered(skill_id: &str) -> Result<(), String> {
    if list_skills().iter().any(|skill| skill.id == skill_id) {
        Ok(())
    } else {
        Err(format!("Unknown skill: {skill_id}"))
    }
}

pub fn execute_skill(
    app: &AppHandle,
    skill_id: &str,
    timestamp: &str,
) -> Result<SkillExecutionResult, String> {
    ensure_registered(skill_id)?;

    let emitted_event = format!("skill:{skill_id}");
    app.emit(&emitted_event, timestamp)
        .map_err(|e| format!("Failed to emit skill event: {e}"))?;

    Ok(SkillExecutionResult {
        skill_id: skill_id.to_string(),
        emitted_event,
        accepted: true,
    })
}
