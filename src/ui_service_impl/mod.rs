//! Real implementation of [`AppService`] — wraps actual backend modules.
//!
//! The trait impl block below is a thin delegator: each method forwards to a
//! domain-specific free function in one of the submodules.  This keeps the
//! method bodies with their related helpers instead of in one 600-line file.
//!
//! Submodules:
//! - [`agents`] — agent CRUD, objectives, behavior, per-agent skill toggles.
//! - [`pending`] — pending-reply queue (load / approve / reject / edit).
//! - [`workflow`] — workflow lifecycle + LLM stage generation.
//! - [`integrations`] — Telegram auth, Teams, MCP, Meeting, Chat, Research, Skills.
//! - [`system`] — logs, task tracker, system status, memory, persona, LLM, config, toasts.

mod agents;
mod integrations;
mod pending;
mod system;
mod workflow;

use std::sync::Mutex;

use crate::persona::TaskTracker;
use crate::telegram_auth::TelegramAuthState;
use crate::ui_service::*;

pub struct RealService {
    pub tracker: TaskTracker,
    pub tg_auth: TelegramAuthState,
    toasts: Mutex<Vec<ToastEvent>>,
    toast_history: Mutex<Vec<ToastEvent>>,
}

impl RealService {
    pub fn new(tracker: TaskTracker, tg_auth: TelegramAuthState) -> Self {
        Self {
            tracker,
            tg_auth,
            toasts: Mutex::new(Vec::new()),
            toast_history: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn push_toast(&self, level: ToastLevel, text: impl Into<String>) {
        if let Ok(mut t) = self.toasts.lock() {
            t.push(ToastEvent { level, text: text.into() });
        }
    }

    /// Drain pending toasts and record them into history (trimmed to 50).
    pub(super) fn drain_toasts(&self) -> Vec<ToastEvent> {
        let new = self
            .toasts
            .lock()
            .ok()
            .map(|mut t| std::mem::take(&mut *t))
            .unwrap_or_default();
        if let Ok(mut h) = self.toast_history.lock() {
            h.extend(new.iter().cloned());
            if h.len() > 50 {
                let n = h.len() - 50;
                h.drain(..n);
            }
        }
        new
    }

    pub(super) fn toast_history_snapshot(&self) -> Vec<ToastEvent> {
        self.toast_history.lock().ok().map(|h| h.clone()).unwrap_or_default()
    }
}

impl AgentService for RealService {
    fn list_agents(&self) -> Vec<AgentSummary> { agents::list_agents(self) }
    fn agent_detail(&self, id: &str) -> Option<AgentDetailView> { agents::agent_detail(self, id) }
    fn create_agent(&self, id: &str, name: &str) { agents::create_agent(self, id, name) }
    fn rename_agent(&self, id: &str, name: &str) { agents::rename_agent(self, id, name) }
    fn toggle_agent(&self, id: &str, enabled: bool) { agents::toggle_agent(self, id, enabled) }
    fn delete_agent(&self, id: &str) { agents::delete_agent(self, id) }
    fn add_objective(&self, id: &str, text: &str) { agents::add_objective(self, id, text) }
    fn remove_objective(&self, id: &str, index: usize) { agents::remove_objective(self, id, index) }
    fn set_remote_ai(&self, id: &str, allowed: bool) { agents::set_remote_ai(self, id, allowed) }
    fn set_behavior(&self, id: &str, enabled: bool, min_delay: u64, max_delay: u64, max_hour: u32, max_day: u32) {
        agents::set_behavior(self, id, enabled, min_delay, max_delay, max_hour, max_day)
    }
    fn toggle_skill(&self, id: &str, skill_id: &str, enabled: bool) { agents::toggle_skill(self, id, skill_id, enabled) }
    fn disabled_skills(&self, id: &str) -> Vec<String> { agents::disabled_skills(self, id) }
}

impl PendingReplyService for RealService {
    fn pending_count(&self, id: &str) -> usize { pending::pending_count(self, id) }
    fn load_pending(&self, id: &str) -> Vec<PendingReplyView> { pending::load_pending(self, id) }
    fn approve_reply(&self, id: &str, reply_id: &str) { pending::approve_reply(self, id, reply_id) }
    fn reject_reply(&self, id: &str, reply_id: &str) { pending::reject_reply(self, id, reply_id) }
    fn edit_draft(&self, id: &str, reply_id: &str, new_text: &str) { pending::edit_draft(self, id, reply_id, new_text) }
}

impl WorkflowService for RealService {
    fn workflow_state(&self) -> Option<WorkflowView> { workflow::workflow_state(self) }
    fn workflow_create(&self, feature: &str, description: &str) { workflow::workflow_create(self, feature, description) }
    fn workflow_advance(&self) -> bool { workflow::workflow_advance(self) }
    fn workflow_stage_prompt(&self) -> Option<String> { workflow::workflow_stage_prompt(self) }
    fn workflow_reset(&self) { workflow::workflow_reset(self) }
    fn workflow_generate(&self) -> Option<String> { workflow::workflow_generate(self) }
    fn workflow_save_output(&self, stage_id: &str, output: &str) { workflow::workflow_save_output(self, stage_id, output) }
}

impl IntegrationService for RealService {
    fn tg_submit_code(&self, code: &str) -> bool { integrations::tg_submit_code(self, code) }
    fn tg_submit_password(&self, pwd: &str) -> bool { integrations::tg_submit_password(self, pwd) }
    fn tg_reconnect(&self) { integrations::tg_reconnect(self) }
    fn start_teams(&self) { integrations::start_teams(self) }
    fn teams_running(&self) -> bool { integrations::teams_running(self) }
    fn mcp_tools(&self) -> Vec<McpToolDetail> { integrations::mcp_tools(self) }
    fn mcp_call(&self, tool: &str, args: &str) -> Result<String, String> { integrations::mcp_call(self, tool, args) }
    fn meeting_active(&self) -> bool { integrations::meeting_active(self) }
    fn meeting_start(&self, participants: Vec<String>) -> String { integrations::meeting_start(self, participants) }
    fn meeting_end(&self) { integrations::meeting_end(self) }
    fn meeting_send(&self, speaker: &str, text: &str) { integrations::meeting_send(self, speaker, text) }
    fn meeting_history(&self) -> Vec<(String, String)> { integrations::meeting_history(self) }
    fn chat_send(&self, id: &str, message: &str) -> String { integrations::chat_send(self, id, message) }
    fn trigger_research(&self, topic: &str, url: Option<&str>) { integrations::trigger_research(self, topic, url) }
    fn execute_skill(&self, skill_id: &str, input: &str) -> String { integrations::execute_skill(self, skill_id, input) }
}

impl SystemService for RealService {
    fn recent_tasks(&self, limit: usize) -> Vec<TaskView> { system::recent_tasks(self, limit) }
    fn log_version(&self) -> usize { system::log_version(self) }
    fn log_recent(&self, limit: usize) -> Vec<LogLine> { system::log_recent(self, limit) }
    fn log_len(&self) -> usize { system::log_len(self) }
    fn log_clear(&self) { system::log_clear(self) }
    fn system_status(&self) -> SystemStatus { system::system_status(self) }
    fn search_memory(&self, query: &str, limit: usize) -> Vec<String> { system::search_memory(self, query, limit) }
    fn persona_name(&self) -> String { system::persona_name(self) }
    fn set_persona_name(&self, name: &str) { system::set_persona_name(self, name) }
    fn persona_objectives(&self) -> Vec<String> { system::persona_objectives(self) }
    fn set_persona_objectives(&self, objectives: Vec<String>) { system::set_persona_objectives(self, objectives) }
    fn persona_voice(&self) -> String { system::persona_voice(self) }
    fn set_persona_voice(&self, voice: &str) { system::set_persona_voice(self, voice) }
    fn available_models(&self) -> Vec<String> { system::available_models(self) }
    fn set_main_model(&self, model: &str) { system::set_main_model(self, model) }
    fn export_config(&self) -> String { system::export_config(self) }
    fn import_config(&self, yaml: &str) -> Result<(), String> { system::import_config(self, yaml) }
    fn poll_toasts(&self) -> Vec<ToastEvent> { system::poll_toasts(self) }
    fn toast_history(&self) -> Vec<ToastEvent> { system::toast_history(self) }
}

// `impl AppService for RealService` is satisfied automatically by the blanket
// impl in `ui_service` — no explicit block needed.
