//! Real implementation of [`AppService`] — wraps actual backend modules.

use crate::persona::TaskTracker;
use crate::telegram_auth::TelegramAuthState;
use crate::ui_service::*;

/// Production service backed by real backend modules.
pub struct RealService {
    pub tracker: TaskTracker,
    pub tg_auth: TelegramAuthState,
}

impl AppService for RealService {
    // ── Agents ───────────────────────────────────────────────────────────────

    fn list_agents(&self) -> Vec<AgentSummary> {
        let file = crate::agent_config::AgentsFile::load().unwrap_or_default();
        file.agents.iter().map(|a| {
            let platform = if a.channel.as_ref().and_then(|c| c.telegram.as_ref()).is_some() {
                "telegram"
            } else if a.channel.as_ref().and_then(|c| c.teams.as_ref()).is_some() {
                "teams"
            } else {
                "ui_only"
            };
            AgentSummary {
                id: a.id.clone(),
                name: a.identity.name.clone(),
                enabled: a.enabled,
                platform: platform.to_string(),
            }
        }).collect()
    }

    fn agent_detail(&self, agent_id: &str) -> Option<AgentDetailView> {
        let file = crate::agent_config::AgentsFile::load().unwrap_or_default();
        let a = file.agents.iter().find(|a| a.id == agent_id)?;
        let platform = if a.channel.as_ref().and_then(|c| c.telegram.as_ref()).is_some() {
            "telegram"
        } else if a.channel.as_ref().and_then(|c| c.teams.as_ref()).is_some() {
            "teams"
        } else {
            "ui_only"
        };
        Some(AgentDetailView {
            id: a.id.clone(),
            name: a.identity.name.clone(),
            enabled: a.enabled,
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

    // ── Pending replies ──────────────────────────────────────────────────────

    fn pending_count(&self, agent_id: &str) -> usize {
        crate::pending_reply::load_pending(agent_id)
            .into_iter()
            .filter(|r| r.status == crate::pending_reply::PendingStatus::Pending)
            .count()
    }

    fn load_pending(&self, agent_id: &str) -> Vec<PendingReplyView> {
        crate::pending_reply::load_pending(agent_id)
            .into_iter()
            .filter(|r| r.status == crate::pending_reply::PendingStatus::Pending)
            .map(|r| PendingReplyView {
                id: r.id,
                agent_id: r.agent_id,
                peer_name: r.peer_name,
                original_message: r.original_message,
                draft_reply: r.draft_reply,
                created_at: r.created_at,
            })
            .collect()
    }

    fn approve_reply(&self, agent_id: &str, reply_id: &str) {
        crate::pending_reply::update_status(agent_id, reply_id, crate::pending_reply::PendingStatus::Approved);
    }

    fn reject_reply(&self, agent_id: &str, reply_id: &str) {
        crate::pending_reply::update_status(agent_id, reply_id, crate::pending_reply::PendingStatus::Rejected);
    }

    // ── Tasks ────────────────────────────────────────────────────────────────

    fn recent_tasks(&self, limit: usize) -> Vec<TaskView> {
        self.tracker
            .read_last_n(limit)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.event != "heartbeat")
            .rev()
            .map(|e| TaskView {
                timestamp: e.timestamp.clone(),
                event: e.event.clone(),
                status: e.status.clone(),
                reason: e.reason.clone(),
            })
            .collect()
    }

    // ── Log ──────────────────────────────────────────────────────────────────

    fn log_version(&self) -> usize {
        crate::log_buffer::version()
    }

    fn log_recent(&self, limit: usize) -> Vec<LogLine> {
        crate::log_buffer::recent(limit)
            .into_iter()
            .map(|text| {
                let level = classify_log_level(&text);
                LogLine { text, level }
            })
            .collect()
    }

    fn log_len(&self) -> usize {
        crate::log_buffer::len()
    }

    fn log_clear(&self) {
        crate::log_buffer::clear();
    }

    // ── System ───────────────────────────────────────────────────────────────

    fn system_status(&self) -> SystemStatus {
        let tg_status_enum = self.tg_auth.status();
        let tg_connected = matches!(tg_status_enum, crate::telegram_auth::TelegramStatus::Connected);

        let llm = crate::llm::shared_llm();
        let router = crate::llm::shared_router_llm();

        let mcp_tools = crate::mcp_client::get_discovered_tools()
            .iter()
            .map(|t| McpToolInfo {
                name: t.registry_name(),
                description: t.description.clone(),
            })
            .collect();

        let skills = crate::skills::list_skills()
            .iter()
            .map(|s| SkillInfo {
                name: s.name.clone(),
                category: s.category.clone(),
                description: s.description.clone(),
            })
            .collect();

        SystemStatus {
            telegram_connected: tg_connected,
            telegram_status: format!("{:?}", tg_status_enum),
            rpc_running: crate::rpc_server::is_running(),
            llm: LlmInfo {
                main_model: llm.model.clone(),
                main_backend: llm.backend_name().to_string(),
                router_model: router.model.clone(),
                router_backend: router.backend_name().to_string(),
                is_remote: llm.is_remote(),
            },
            mcp_tools,
            skills,
        }
    }

    // ── Workflow ──────────────────────────────────────────────────────────────

    fn workflow_state(&self) -> Option<WorkflowView> {
        let state = crate::workflow::WorkflowState::load()?;
        let stages = crate::workflow::STAGES
            .iter()
            .map(|s| StageView {
                id: s.id.to_string(),
                label: s.label.to_string(),
                desc: s.desc.to_string(),
                status: match state.stage_status(s.id) {
                    crate::workflow::StageStatus::Done => StageStatusView::Done,
                    crate::workflow::StageStatus::Current => StageStatusView::Current,
                    crate::workflow::StageStatus::Pending => StageStatusView::Pending,
                },
            })
            .collect();
        Some(WorkflowView {
            feature: state.feature.clone(),
            description: state.description.clone(),
            skill_id: state.skill_id.clone(),
            current_stage: state.current_stage.clone(),
            started_at: state.started_at.clone(),
            stages,
            all_done: state.all_done(),
        })
    }

    fn workflow_create(&self, feature: &str, description: &str) {
        let skill_id = feature.to_lowercase().replace(' ', "_");
        let state = crate::workflow::WorkflowState::new(feature, description, &skill_id);
        state.save();
    }

    fn workflow_reset(&self) {
        let _ = std::fs::remove_file("data/workflow.json");
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn classify_log_level(line: &str) -> LogLevel {
    let lower = line.to_lowercase();
    if line.contains("[ERROR]") || lower.contains("error") || lower.contains("failed") {
        LogLevel::Error
    } else if line.contains("[WARN]") || lower.contains("warn") {
        LogLevel::Warn
    } else if line.contains("[telegram]") || line.contains("[tg]") {
        LogLevel::Telegram
    } else if line.contains("[researcher]") {
        LogLevel::Research
    } else if line.contains("[followup]") {
        LogLevel::Followup
    } else if line.contains("[coding]") || line.contains("[adk]") {
        LogLevel::Coding
    } else if line.contains("[teams]") {
        LogLevel::Teams
    } else {
        LogLevel::Normal
    }
}
