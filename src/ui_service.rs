//! Service trait — the single boundary between UI and backend.
//!
//! All UI components access backend functionality exclusively through this trait.
//! This enables:
//! - **Testing**: MockService returns canned data, no file I/O needed
//! - **Web mode**: Service can be backed by HTTP/RPC instead of direct calls
//! - **Isolation**: UI crate has zero imports of backend modules
//!
//! # Usage in Dioxus components
//! ```ignore
//! let svc = use_context::<Signal<Box<dyn AppService>>>();
//! let pending = svc.read().load_pending("agent_1");
//! ```

use serde::{Deserialize, Serialize};

// ── Data types (shared between UI and backend) ──────────────────────────────

/// Summary of one agent for UI display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub platform: String, // "telegram", "teams", "ui_only"
}

/// One pending reply awaiting approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingReplyView {
    pub id: String,
    pub agent_id: String,
    pub peer_name: String,
    pub original_message: String,
    pub draft_reply: String,
    pub created_at: String,
}

/// One task entry for the activity log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskView {
    pub timestamp: String,
    pub event: String,
    pub status: Option<String>,
    pub reason: Option<String>,
}

/// One log line with a severity level.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub text: String,
    pub level: LogLevel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Telegram,
    Research,
    Followup,
    Coding,
    Teams,
    Normal,
}

/// LLM model info for system panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmInfo {
    pub main_model: String,
    pub main_backend: String,
    pub router_model: String,
    pub router_backend: String,
    pub is_remote: bool,
}

/// External MCP tool info.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

/// Skill info.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub category: String,
    pub description: String,
}

/// System status snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemStatus {
    pub telegram_connected: bool,
    pub telegram_status: String,
    pub rpc_running: bool,
    pub llm: LlmInfo,
    pub mcp_tools: Vec<McpToolInfo>,
    pub skills: Vec<SkillInfo>,
}

/// Workflow stage.
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

// ── Agent config for settings ───────────────────────────────────────────────

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
    pub kpi_labels: Vec<(String, String)>, // (label, unit)
}

// ── Service trait ────────────────────────────────────────────────────────────

/// The single interface between UI and backend. All UI data flows through here.
pub trait AppService: Send + Sync + 'static {
    // Agents
    fn list_agents(&self) -> Vec<AgentSummary>;
    fn agent_detail(&self, agent_id: &str) -> Option<AgentDetailView>;

    // Pending replies
    fn pending_count(&self, agent_id: &str) -> usize;
    fn load_pending(&self, agent_id: &str) -> Vec<PendingReplyView>;
    fn approve_reply(&self, agent_id: &str, reply_id: &str);
    fn reject_reply(&self, agent_id: &str, reply_id: &str);

    // Tasks / activity
    fn recent_tasks(&self, limit: usize) -> Vec<TaskView>;

    // Log
    fn log_version(&self) -> usize;
    fn log_recent(&self, limit: usize) -> Vec<LogLine>;
    fn log_len(&self) -> usize;
    fn log_clear(&self);

    // System
    fn system_status(&self) -> SystemStatus;

    // Workflow
    fn workflow_state(&self) -> Option<WorkflowView>;
    fn workflow_create(&self, feature: &str, description: &str);
    fn workflow_reset(&self);
}
