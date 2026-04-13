//! Real implementation of [`AppService`] — wraps actual backend modules.

use std::sync::Mutex;

use crate::persona::TaskTracker;
use crate::telegram_auth::TelegramAuthState;
use crate::ui_service::*;

pub struct RealService {
    pub tracker: TaskTracker,
    pub tg_auth: TelegramAuthState,
    toasts: Mutex<Vec<ToastEvent>>,
}

impl RealService {
    pub fn new(tracker: TaskTracker, tg_auth: TelegramAuthState) -> Self {
        Self { tracker, tg_auth, toasts: Mutex::new(Vec::new()) }
    }

    fn push_toast(&self, level: ToastLevel, text: impl Into<String>) {
        if let Ok(mut t) = self.toasts.lock() {
            t.push(ToastEvent { level, text: text.into() });
        }
    }
}

impl AppService for RealService {
    // ── Read: Agents ─────────────────────────────────────────────────────────

    fn list_agents(&self) -> Vec<AgentSummary> {
        let file = crate::agent_config::AgentsFile::load().unwrap_or_default();
        file.agents.iter().map(|a| {
            let platform = if a.channel.as_ref().and_then(|c| c.telegram.as_ref()).is_some() {
                "telegram"
            } else if a.channel.as_ref().and_then(|c| c.teams.as_ref()).is_some() {
                "teams"
            } else { "ui_only" };
            let live_status = if !a.enabled { "idle" }
                else if platform == "telegram" {
                    match self.tg_auth.status() {
                        crate::telegram_auth::TelegramStatus::Connected => "connected",
                        crate::telegram_auth::TelegramStatus::Disconnected { .. } => "reconnecting",
                        _ => "waiting",
                    }
                } else { "idle" };
            AgentSummary { id: a.id.clone(), name: a.identity.name.clone(), enabled: a.enabled,
                platform: platform.to_string(), live_status: live_status.to_string() }
        }).collect()
    }

    fn agent_detail(&self, agent_id: &str) -> Option<AgentDetailView> {
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

    // ── Read: Pending replies ────────────────────────────────────────────────

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
                id: r.id, agent_id: r.agent_id, peer_name: r.peer_name,
                original_message: r.original_message, draft_reply: r.draft_reply, created_at: r.created_at,
            })
            .collect()
    }

    // ── Read: Tasks ──────────────────────────────────────────────────────────

    fn recent_tasks(&self, limit: usize) -> Vec<TaskView> {
        self.tracker.read_last_n(limit).unwrap_or_default()
            .into_iter()
            .filter(|e| e.event != "heartbeat")
            .rev()
            .map(|e| TaskView { timestamp: e.timestamp, event: e.event, status: e.status, reason: e.reason })
            .collect()
    }

    // ── Read: Log ────────────────────────────────────────────────────────────

    fn log_version(&self) -> usize { crate::log_buffer::version() }

    fn log_recent(&self, limit: usize) -> Vec<LogLine> {
        crate::log_buffer::recent(limit).into_iter()
            .map(|text| { let level = classify_log_level(&text); LogLine { text, level } })
            .collect()
    }

    fn log_len(&self) -> usize { crate::log_buffer::len() }

    // ── Read: System ─────────────────────────────────────────────────────────

    fn system_status(&self) -> SystemStatus {
        let tg = self.tg_auth.status();
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

    // ── Read: Workflow ────────────────────────────────────────────────────────

    fn workflow_state(&self) -> Option<WorkflowView> {
        let state = crate::workflow::WorkflowState::load()?;
        let stages = crate::workflow::STAGES.iter().map(|s| StageView {
            id: s.id.to_string(), label: s.label.to_string(), desc: s.desc.to_string(),
            status: match state.stage_status(s.id) {
                crate::workflow::StageStatus::Done => StageStatusView::Done,
                crate::workflow::StageStatus::Current => StageStatusView::Current,
                crate::workflow::StageStatus::Pending => StageStatusView::Pending,
            },
        }).collect();
        let all_done = state.all_done();
        Some(WorkflowView {
            feature: state.feature, description: state.description, skill_id: state.skill_id,
            current_stage: state.current_stage, started_at: state.started_at, stages, all_done,
        })
    }

    // ── Read: Memory ─────────────────────────────────────────────────────────

    fn search_memory(&self, query: &str, limit: usize) -> Vec<String> {
        crate::memory::memory_search(query, limit, "").unwrap_or_default()
    }

    // ── Write: Pending ───────────────────────────────────────────────────────

    fn approve_reply(&self, agent_id: &str, reply_id: &str) {
        crate::pending_reply::update_status(agent_id, reply_id, crate::pending_reply::PendingStatus::Approved);
        self.push_toast(ToastLevel::Success, "已核准");
    }

    fn reject_reply(&self, agent_id: &str, reply_id: &str) {
        crate::pending_reply::update_status(agent_id, reply_id, crate::pending_reply::PendingStatus::Rejected);
    }

    fn log_clear(&self) { crate::log_buffer::clear(); }

    // ── Write: Workflow ──────────────────────────────────────────────────────

    fn workflow_create(&self, feature: &str, description: &str) {
        let skill_id = feature.to_lowercase().replace(' ', "_");
        let state = crate::workflow::WorkflowState::new(feature, description, &skill_id);
        state.save();
        self.push_toast(ToastLevel::Success, format!("Workflow「{feature}」已建立"));
    }

    fn workflow_reset(&self) {
        let _ = std::fs::remove_file("data/workflow.json");
    }

    // ── Write: Agent config ──────────────────────────────────────────────────

    fn rename_agent(&self, agent_id: &str, new_name: &str) {
        if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
            if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
                a.identity.name = new_name.to_string();
                let _ = file.save();
                self.push_toast(ToastLevel::Success, format!("已改名為「{new_name}」"));
            }
        }
    }

    fn toggle_agent(&self, agent_id: &str, enabled: bool) {
        if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
            if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
                a.enabled = enabled;
                let _ = file.save();
            }
        }
    }

    fn add_objective(&self, agent_id: &str, text: &str) {
        if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
            if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
                a.objectives.push(text.to_string());
                let _ = file.save();
            }
        }
    }

    fn remove_objective(&self, agent_id: &str, index: usize) {
        if let Ok(mut file) = crate::agent_config::AgentsFile::load() {
            if let Some(a) = file.agents.iter_mut().find(|a| a.id == agent_id) {
                if index < a.objectives.len() {
                    a.objectives.remove(index);
                    let _ = file.save();
                }
            }
        }
    }

    // ── Telegram auth ────────────────────────────────────────────────────────

    fn tg_submit_code(&self, code: &str) -> bool {
        self.tg_auth.submit_code(code.to_string())
    }

    fn tg_submit_password(&self, password: &str) -> bool {
        self.tg_auth.submit_password(password.to_string())
    }

    fn tg_reconnect(&self) {
        self.tg_auth.trigger_reconnect();
    }

    // ── MCP Tools ─────────────────────────────────────────────────────────────

    fn mcp_tools(&self) -> Vec<McpToolDetail> {
        crate::mcp_client::get_discovered_tools().iter().map(|t| {
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
        }).collect()
    }

    fn mcp_call(&self, tool_name: &str, args_json: &str) -> Result<String, String> {
        // Find the tool
        let tools = crate::mcp_client::get_discovered_tools();
        let tool = tools.iter().find(|t| t.registry_name() == tool_name || t.tool_name == tool_name)
            .ok_or_else(|| format!("Tool not found: {tool_name}"))?;

        let args: serde_json::Value = serde_json::from_str(args_json)
            .map_err(|e| format!("Invalid JSON: {e}"))?;

        let url = tool.server_url.clone();
        let name = tool.tool_name.clone();

        // Run synchronously (UI blocks briefly — acceptable for tool calls)
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| "No tokio runtime".to_string())?;
        let result = std::thread::spawn(move || {
            rt.block_on(crate::mcp_client::call_tool(&url, &name, args))
        }).join().map_err(|_| "Thread panic".to_string())??;

        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| format!("{result}")))
    }

    // ── Pending reply editing ────────────────────────────────────────────────

    fn edit_draft(&self, agent_id: &str, reply_id: &str, new_text: &str) {
        let mut replies = crate::pending_reply::load_pending(agent_id);
        if let Some(r) = replies.iter_mut().find(|r| r.id == reply_id) {
            r.draft_reply = new_text.to_string();
        }
        let _ = crate::pending_reply::save_pending(agent_id, &replies);
    }

    // ── Meeting ───────────────────────────────────────────────────────────────

    fn meeting_send(&self, _speaker: &str, _text: &str) {
        // TODO: integrate with meeting module for AI-mediated responses
    }

    // ── Events ───────────────────────────────────────────────────────────────

    fn poll_toasts(&self) -> Vec<ToastEvent> {
        self.toasts.lock().ok().map(|mut t| std::mem::take(&mut *t)).unwrap_or_default()
    }
}

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
