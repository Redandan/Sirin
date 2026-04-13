//! Service trait — the single boundary between UI and backend.
//!
//! All UI components access backend functionality exclusively through this trait.
//! AI reads the trait to understand what data/actions are available.

use serde::{Deserialize, Serialize};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub platform: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingReplyView {
    pub id: String,
    pub agent_id: String,
    pub peer_name: String,
    pub original_message: String,
    pub draft_reply: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskView {
    pub timestamp: String,
    pub event: String,
    pub status: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub text: String,
    pub level: LogLevel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogLevel { Error, Warn, Info, Telegram, Research, Followup, Coding, Teams, Normal }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmInfo {
    pub main_model: String,
    pub main_backend: String,
    pub router_model: String,
    pub router_backend: String,
    pub is_remote: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub category: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SystemStatus {
    pub telegram_connected: bool,
    pub telegram_status: String,
    pub rpc_running: bool,
    pub llm: LlmInfo,
    pub mcp_tools: Vec<McpToolInfo>,
    pub skills: Vec<SkillInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowView {
    pub feature: String,
    pub description: String,
    pub skill_id: String,
    pub current_stage: String,
    pub started_at: String,
    pub stages: Vec<StageView>,
    pub all_done: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageView {
    pub id: String,
    pub label: String,
    pub desc: String,
    pub status: StageStatusView,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StageStatusView { Done, Current, Pending }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDetailView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub platform: String,
    pub professional_tone: String,
    pub disable_remote_ai: bool,
    pub objectives: Vec<String>,
    pub human_behavior_enabled: bool,
    pub min_reply_delay: u64,
    pub max_reply_delay: u64,
    pub max_per_hour: u32,
    pub max_per_day: u32,
    pub kpi_labels: Vec<(String, String)>,
}

/// Toast notification pushed from service to UI.
#[derive(Debug, Clone)]
pub struct ToastEvent {
    pub level: ToastLevel,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToastLevel { Info, Success, Error }

// ── Service trait ────────────────────────────────────────────────────────────

pub trait AppService: Send + Sync + 'static {
    // ── Read ─────────────────────────────────────────────────────────────────

    fn list_agents(&self) -> Vec<AgentSummary>;
    fn agent_detail(&self, agent_id: &str) -> Option<AgentDetailView>;

    fn pending_count(&self, agent_id: &str) -> usize;
    fn load_pending(&self, agent_id: &str) -> Vec<PendingReplyView>;

    fn recent_tasks(&self, limit: usize) -> Vec<TaskView>;

    fn log_version(&self) -> usize;
    fn log_recent(&self, limit: usize) -> Vec<LogLine>;
    fn log_len(&self) -> usize;

    fn system_status(&self) -> SystemStatus;

    fn workflow_state(&self) -> Option<WorkflowView>;

    fn search_memory(&self, query: &str, limit: usize) -> Vec<String>;

    // ── Write ────────────────────────────────────────────────────────────────

    fn approve_reply(&self, agent_id: &str, reply_id: &str);
    fn reject_reply(&self, agent_id: &str, reply_id: &str);

    fn log_clear(&self);

    fn workflow_create(&self, feature: &str, description: &str);
    fn workflow_reset(&self);

    fn rename_agent(&self, agent_id: &str, new_name: &str);
    fn toggle_agent(&self, agent_id: &str, enabled: bool);
    fn add_objective(&self, agent_id: &str, text: &str);
    fn remove_objective(&self, agent_id: &str, index: usize);

    // ── Telegram auth ────────────────────────────────────────────────────────

    fn tg_submit_code(&self, code: &str) -> bool;
    fn tg_submit_password(&self, password: &str) -> bool;
    fn tg_reconnect(&self);

    // ── Meeting ───────────────────────────────────────────────────────────────

    fn meeting_send(&self, speaker: &str, text: &str);

    // ── Events (polled by UI each frame) ─────────────────────────────────────

    fn poll_toasts(&self) -> Vec<ToastEvent>;
}
