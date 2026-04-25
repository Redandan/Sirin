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
    /// Real-time status: "connected", "reconnecting", "error", "idle"
    pub live_status: String,
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

/// MCP tool with full schema details for UI display and execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolDetail {
    pub server_name: String,
    pub tool_name: String,
    pub registry_name: String,
    pub description: String,
    /// JSON Schema properties as human-readable param list.
    pub params: Vec<(String, String)>, // (name, type)
}

/// Toast notification pushed from service to UI.
#[derive(Debug, Clone)]
pub struct ToastEvent {
    pub level: ToastLevel,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToastLevel { Info, Success, Error }

/// Config-check issue for the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigIssueView {
    pub severity: ConfigSeverity,
    pub category: String,
    pub message: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSeverity { Ok, Info, Warning, Error }

/// AI-proposed single field change.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFixView {
    pub file: String,
    pub field_path: String,
    pub current_value: String,
    pub new_value: String,
    pub reason: String,
}

/// AI-generated config advice.
#[derive(Debug, Clone, PartialEq)]
pub struct AiAdviceView {
    pub analysis: String,
    pub proposed_fixes: Vec<ConfigFixView>,
}

// ── Multi-Agent team types ────────────────────────────────────────────────────

/// 單一佇列任務的 UI 視圖。
#[derive(Debug, Clone, PartialEq)]
pub struct TeamTaskView {
    pub id: String,
    pub description: String,
    pub status: String,           // "queued" | "running" | "done" | "failed"
    pub result: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

/// PM / Engineer / Tester 其中一人的狀態。
#[derive(Debug, Clone, PartialEq)]
pub struct TeamMemberView {
    pub role: String,
    pub session_id: Option<String>,
    pub turns: u32,
    pub resume_cmd: String,
}

/// Live token burn snapshot from the last N seconds of squad worker sessions.
#[derive(Debug, Clone, Default)]
pub struct TokenUsageView {
    pub window_secs:     u64,
    pub api_calls:       u64,
    pub tokens_per_min:  u64,
    pub input_per_min:   u64,
    pub output_per_min:  u64,
    pub cache_r_per_min: u64,
    pub cache_w_per_min: u64,
    pub cost_per_hour:   f64,
    pub cache_hit_pct:   f64,
}

/// 整個小隊的即時狀態快照。
#[derive(Debug, Clone, PartialEq)]
pub struct TeamDashView {
    pub pm: TeamMemberView,
    pub engineer: TeamMemberView,
    pub tester: TeamMemberView,
    pub worker_running: bool,
    pub queued: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
}

/// One saved dry-run preview (would-be GitHub comment) for the panel list.
#[derive(Debug, Clone, PartialEq)]
pub struct DryRunPreviewView {
    pub task_id:   String,
    pub issue_url: String,
    pub success:   bool,
    pub saved_at:  String,
    pub body:      String,
}

/// Read-only snapshot of a GitHub issue for the verification form's preview.
#[derive(Debug, Clone, PartialEq)]
pub struct GhIssueView {
    pub title:  String,
    pub body:   String,
    pub labels: Vec<String>,
    pub url:    String,
}

/// One test run row for the Test Dashboard panel.
#[derive(Debug, Clone, PartialEq)]
pub struct TestRunView {
    /// YAML test id (or "adhoc_…" for ad-hoc runs).
    pub test_id:     String,
    /// "passed" | "failed" | "timeout" | "error" | "running" | "queued"
    pub status:      String,
    /// RFC-3339 timestamp of when the run started.
    pub started_at:  String,
    /// Wall-clock duration in milliseconds (None for still-active runs).
    pub duration_ms: Option<u64>,
    /// Short AI analysis or current action (truncated by UI).
    pub analysis:    Option<String>,
}

// ── Service traits ───────────────────────────────────────────────────────────
//
// Split by domain so UI consumers (and future alternative implementations) can
// target the narrowest surface they actually need, rather than the full
// 50-method god-trait.  `AppService` is the supertrait union; a blanket impl
// means anything that implements all sub-traits automatically satisfies it,
// and `Arc<dyn AppService>` behaves exactly as before for the UI layer.

/// Agent CRUD, objectives, human-behavior config, per-agent skill toggles.
pub trait AgentService: Send + Sync + 'static {
    fn list_agents(&self) -> Vec<AgentSummary>;
    fn agent_detail(&self, agent_id: &str) -> Option<AgentDetailView>;
    fn create_agent(&self, id: &str, name: &str);
    fn rename_agent(&self, agent_id: &str, new_name: &str);
    fn toggle_agent(&self, agent_id: &str, enabled: bool);
    fn delete_agent(&self, agent_id: &str);
    fn add_objective(&self, agent_id: &str, text: &str);
    fn remove_objective(&self, agent_id: &str, index: usize);
    fn set_remote_ai(&self, agent_id: &str, allowed: bool);
    fn set_behavior(&self, agent_id: &str, enabled: bool, min_delay: u64, max_delay: u64, max_hour: u32, max_day: u32);
    fn toggle_skill(&self, agent_id: &str, skill_id: &str, enabled: bool);
    fn disabled_skills(&self, agent_id: &str) -> Vec<String>;
}

/// Pending-reply queue read + mutation.
pub trait PendingReplyService: Send + Sync + 'static {
    fn pending_count(&self, agent_id: &str) -> usize;
    fn load_pending(&self, agent_id: &str) -> Vec<PendingReplyView>;
    fn approve_reply(&self, agent_id: &str, reply_id: &str);
    fn reject_reply(&self, agent_id: &str, reply_id: &str);
    fn edit_draft(&self, agent_id: &str, reply_id: &str, new_text: &str);
}

/// Workflow lifecycle + LLM-driven stage output generation.
pub trait WorkflowService: Send + Sync + 'static {
    fn workflow_state(&self) -> Option<WorkflowView>;
    fn workflow_create(&self, feature: &str, description: &str);
    fn workflow_advance(&self) -> bool;
    fn workflow_stage_prompt(&self) -> Option<String>;
    fn workflow_reset(&self);
    /// Call LLM with the current stage prompt and return the generated text.
    fn workflow_generate(&self) -> Option<String>;
    /// Save AI output to the current stage.
    fn workflow_save_output(&self, stage_id: &str, output: &str);
}

/// External integrations — Telegram auth, Teams session, MCP tools, meeting
/// room, direct chat, research triggers, and local skill execution.
pub trait IntegrationService: Send + Sync + 'static {
    fn tg_submit_code(&self, code: &str) -> bool;
    fn tg_submit_password(&self, password: &str) -> bool;
    fn tg_reconnect(&self);

    fn start_teams(&self);
    fn teams_running(&self) -> bool;

    /// List external MCP tools with full details.
    fn mcp_tools(&self) -> Vec<McpToolDetail>;
    /// Execute an MCP tool by name with JSON arguments.
    fn mcp_call(&self, tool_name: &str, args_json: &str) -> Result<String, String>;

    fn meeting_active(&self) -> bool;
    fn meeting_start(&self, participants: Vec<String>) -> String;
    fn meeting_end(&self);
    fn meeting_send(&self, speaker: &str, text: &str);
    fn meeting_history(&self) -> Vec<(String, String)>;

    /// Send a message to the agent and get a reply (blocking).
    fn chat_send(&self, agent_id: &str, message: &str) -> String;

    fn trigger_research(&self, topic: &str, url: Option<&str>);

    fn execute_skill(&self, skill_id: &str, input: &str) -> String;
}

/// App-level state — logs, task tracker, system status, memory search,
/// persona/LLM/config editing, and the toast event buffer.
pub trait SystemService: Send + Sync + 'static {
    fn recent_tasks(&self, limit: usize) -> Vec<TaskView>;

    fn log_version(&self) -> usize;
    fn log_recent(&self, limit: usize) -> Vec<LogLine>;
    fn log_len(&self) -> usize;
    fn log_clear(&self);

    fn system_status(&self) -> SystemStatus;

    fn search_memory(&self, query: &str, limit: usize) -> Vec<String>;

    fn persona_name(&self) -> String;
    fn set_persona_name(&self, name: &str);
    fn persona_objectives(&self) -> Vec<String>;
    fn set_persona_objectives(&self, objectives: Vec<String>);
    fn persona_voice(&self) -> String;
    fn set_persona_voice(&self, voice: &str);

    fn available_models(&self) -> Vec<String>;
    fn set_main_model(&self, model: &str);

    fn export_config(&self) -> String;
    fn import_config(&self, yaml: &str) -> Result<(), String>;

    fn poll_toasts(&self) -> Vec<ToastEvent>;
    fn toast_history(&self) -> Vec<ToastEvent>;

    /// Run configuration diagnostics — returns list of issues.
    fn config_check(&self) -> Vec<ConfigIssueView>;

    /// Ask LLM to analyze config and propose fixes (blocking — may take seconds).
    /// Does NOT modify any files.  Returns Err on LLM/parse failure.
    fn config_ai_analyze(&self) -> Result<AiAdviceView, String>;

    /// Apply approved fixes.  Backs up each file to .bak.TIMESTAMP first.
    /// Returns list of applied fix descriptions or Err on failure.
    fn config_apply_fixes(&self, fixes: Vec<ConfigFixView>) -> Result<Vec<String>, String>;
}

/// 開發小隊狀態感知 — 佇列讀寫、Worker 控制、成員重置。
pub trait MultiAgentService: Send + Sync + 'static {
    /// 取得整個小隊的即時狀態（含 Worker 是否在跑、佇列計數）。
    fn team_dashboard(&self) -> TeamDashView;
    /// 取得所有任務列表（最新在前）。
    fn team_queue(&self) -> Vec<TeamTaskView>;
    /// 加入新任務，回傳任務 ID。
    fn team_enqueue(&self, description: &str) -> String;
    /// 啟動背景 Worker（idempotent）。
    fn team_start_worker(&self);
    /// 清除所有 Done / Failed 任務。
    fn team_clear_completed(&self);
    /// 重置指定角色的 session（開新對話）。
    fn team_reset_member(&self, role: &str);
    /// Live token burn snapshot over the given window (default 300s = 5 min).
    fn team_token_usage(&self, window_secs: u64) -> TokenUsageView;

    // ── GitHub bridge (dev_team_*) ────────────────────────────────────────
    /// Read a GitHub issue (title/body/labels) without enqueueing — used by
    /// the verification panel to show a preview before commit.
    fn dev_team_read_issue(&self, gh_repo: &str, issue_number: u32) -> Result<GhIssueView, String>;
    /// Enqueue an issue as a TeamTask. Returns the new task_id.
    /// `dry_run=true` is the safe default: review goes to preview JSONL, no
    /// `gh issue comment` is invoked when the worker finishes.
    fn dev_team_enqueue_issue(
        &self,
        project_key:  &str,
        gh_repo:      &str,
        issue_number: u32,
        dry_run:      bool,
        priority:     u8,
    ) -> Result<String, String>;
    /// List all saved dry-run previews (newest first).
    fn dev_team_list_previews(&self) -> Vec<DryRunPreviewView>;
    /// Replay (i.e. actually post) a saved preview to its issue.
    fn dev_team_replay_preview(&self, task_id: &str) -> Result<(), String>;
}

/// Browser automation — persistent Chrome session control.
pub trait BrowserService: Send + Sync + 'static {
    fn browser_is_open(&self) -> bool;
    fn browser_open(&self, url: &str, headless: bool);
    fn browser_navigate(&self, url: &str) -> Result<(), String>;
    fn browser_click(&self, selector: &str) -> Result<(), String>;
    fn browser_type(&self, selector: &str, text: &str) -> Result<(), String>;
    fn browser_screenshot(&self) -> Option<Vec<u8>>;
    fn browser_eval(&self, js: &str) -> Result<String, String>;
    fn browser_read(&self, selector: &str) -> Result<String, String>;
    fn browser_close(&self);
    fn browser_url(&self) -> Option<String>;
    fn browser_title(&self) -> Option<String>;
    fn browser_click_point(&self, x: f64, y: f64) -> Result<(), String>;
    fn browser_hover(&self, selector: &str) -> Result<(), String>;
    fn browser_press_key(&self, key: &str) -> Result<(), String>;
    fn browser_wait(&self, selector: &str, timeout_ms: u64) -> Result<(), String>;
    fn browser_exists(&self, selector: &str) -> bool;
    fn browser_select(&self, selector: &str, value: &str) -> Result<(), String>;
    fn browser_scroll(&self, x: f64, y: f64) -> Result<(), String>;
    fn browser_set_viewport(&self, width: u32, height: u32, mobile: bool) -> Result<(), String>;
    fn browser_console(&self, limit: usize) -> String;
    fn browser_tab_count(&self) -> usize;
}

/// Test runner data for the Test Dashboard panel — recent history + active runs.
pub trait TestRunnerService: Send + Sync + 'static {
    /// Most-recent completed runs from SQLite (newest first).
    fn recent_test_runs(&self, limit: usize) -> Vec<TestRunView>;
    /// Currently running or queued runs from the in-memory registry.
    fn active_test_runs(&self) -> Vec<TestRunView>;
}

/// Aggregate trait the UI consumes as `Arc<dyn AppService>`.
///
/// Any type that implements all eight sub-traits automatically gets
/// `AppService` via the blanket impl below — no separate impl block required.
pub trait AppService:
    AgentService + PendingReplyService + WorkflowService + IntegrationService
    + SystemService + BrowserService + MultiAgentService + TestRunnerService
{
}

impl<T> AppService for T where
    T: AgentService
        + PendingReplyService
        + WorkflowService
        + IntegrationService
        + SystemService
        + BrowserService
        + MultiAgentService
        + TestRunnerService
        + ?Sized
{
}
