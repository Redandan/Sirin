//! Native egui/eframe UI for Sirin.
//!
//! Runs on the main thread. Background Tokio tasks (Telegram listener,
//! follow-up worker) communicate via the same shared-state structs they
//! always have — no IPC layer needed.

use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, RichText, ScrollArea, TextEdit,
};
use tokio::runtime::Handle;
use tokio::sync::broadcast;

use crate::events::AgentEvent;
use crate::log_buffer;
use crate::memory::ensure_codebase_index;
use crate::persona::{TaskEntry, TaskTracker};
use crate::researcher::{self, ResearchStatus, ResearchTask};
use crate::telegram_auth::{TelegramAuthState, TelegramStatus};

// ── Tab selector ──────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Dispatch,
    Tasks,
    Research,
    Chat,
    Settings,
    Log,
}

// ── Dispatch task type ────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy, Default)]
#[allow(dead_code)]
enum DispatchTaskType {
    #[default]
    Chat,
    Research,
    Coding,
}

// ── Task filter ───────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum TaskFilter {
    All,
    Running,
    Done,
    Failed,
}

// ── Chat types ────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum ChatRole {
    User,
    Assistant,
}

#[derive(Clone)]
struct ChatMessage {
    role: ChatRole,
    text: String,
}

#[derive(Clone)]
struct AgentConsoleState {
    route: String,
    intent_family: String,
    summary: String,
    ai_details: String,
    steps: Vec<String>,
    recommended_skills: Vec<String>,
    tools: Vec<String>,
    trace: Vec<String>,
    latest_task_summary: String,
    status: String,
}

impl Default for AgentConsoleState {
    fn default() -> Self {
        Self {
            route: String::new(),
            intent_family: String::new(),
            summary: String::new(),
            ai_details: String::new(),
            steps: Vec::new(),
            recommended_skills: Vec::new(),
            tools: Vec::new(),
            trace: Vec::new(),
            latest_task_summary: String::new(),
            status: "Idle".to_string(),
        }
    }
}

impl AgentConsoleState {
    fn snapshot_text(&self) -> String {
        let mut lines = vec![
            format!("Status: {}", self.status),
            format!("Route: {}", self.route),
        ];

        if !self.intent_family.is_empty() {
            lines.push(format!("Intent Family: {}", self.intent_family));
        }
        if !self.summary.is_empty() {
            lines.push(format!("Summary: {}", self.summary));
        }
        if !self.ai_details.is_empty() {
            lines.push(format!("AI: {}", self.ai_details));
        }
        if !self.steps.is_empty() {
            lines.push(format!("Steps: {}", self.steps.join(" → ")));
        }
        if !self.recommended_skills.is_empty() {
            lines.push(format!(
                "Recommended Skills: {}",
                self.recommended_skills.join(", ")
            ));
        }
        if !self.tools.is_empty() {
            lines.push(format!("Tools: {}", self.tools.join(", ")));
        }
        if !self.trace.is_empty() {
            lines.push("Trace:".to_string());
            lines.extend(self.trace.iter().map(|item| format!("- {item}")));
        }
        if !self.latest_task_summary.is_empty() {
            lines.push(format!(
                "Latest Research Summary: {}",
                self.latest_task_summary
            ));
        }

        lines.join("\n")
    }
}

#[derive(Clone, Default)]
struct CodingConsoleState {
    status: String,
    task: String,
    change_summary: String,
    trace: Vec<String>,
    files_modified: Vec<String>,
    diff: Option<String>,
    verified: bool,
    verification_output: Option<String>,
    dry_run: bool,
    outcome: String,
}

impl CodingConsoleState {
    fn snapshot_text(&self) -> String {
        let mut lines = vec![
            format!("Status: {}", self.status),
            format!("Task: {}", self.task),
        ];
        if self.dry_run {
            lines.push("Mode: DRY-RUN (files NOT written)".to_string());
        }
        if !self.change_summary.is_empty() {
            lines.push(format!("Change summary: {}", self.change_summary));
        }
        if !self.files_modified.is_empty() {
            lines.push(format!(
                "Files modified: {}",
                self.files_modified.join(", ")
            ));
        }
        if !self.outcome.is_empty() {
            lines.push(format!("Outcome: {}", self.outcome));
        }
        if self.verified {
            lines.push("✅ cargo check: passed".to_string());
        }
        if let Some(ref vout) = self.verification_output {
            lines.push(format!(
                "Verification: {}",
                vout.chars().take(200).collect::<String>()
            ));
        }
        if !self.trace.is_empty() {
            lines.push("Trace:".to_string());
            lines.extend(self.trace.iter().map(|s| format!("  {s}")));
        }
        if let Some(ref diff) = self.diff {
            lines.push(format!(
                "Diff preview:\n{}",
                diff.chars().take(400).collect::<String>()
            ));
        }
        lines.join("\n")
    }
}

fn chat_history_snapshot(messages: &[ChatMessage]) -> String {
    if messages.is_empty() {
        return "（目前沒有對話內容）".to_string();
    }

    messages
        .iter()
        .map(|msg| {
            let speaker = match msg.role {
                ChatRole::User => "你",
                ChatRole::Assistant => "Sirin",
            };
            format!("{speaker}:\n{}", msg.text)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

fn extract_ai_details(reason: &str) -> String {
    reason
        .split("ai=[")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

fn coding_status_text(resp: &crate::agents::coding_agent::CodingAgentResponse) -> String {
    match resp.result_status {
        crate::agents::coding_agent::CodingResultStatus::Verified => {
            "✅ 完成（已驗證）".to_string()
        }
        crate::agents::coding_agent::CodingResultStatus::DryRunDone => "Dry-run 完成".to_string(),
        crate::agents::coding_agent::CodingResultStatus::Rollback => {
            "↩ 已回滾（待處理）".to_string()
        }
        crate::agents::coding_agent::CodingResultStatus::FollowupNeeded => {
            "⚠️ 需要跟進".to_string()
        }
        crate::agents::coding_agent::CodingResultStatus::Error => "❌ 執行失敗".to_string(),
        crate::agents::coding_agent::CodingResultStatus::Done => {
            if resp.dry_run {
                "Dry-run 完成".to_string()
            } else {
                "完成".to_string()
            }
        }
    }
}

fn task_status_badge(
    status: &str,
    reason: Option<&str>,
    is_summary: bool,
) -> (&'static str, Color32, Color32) {
    let reason = reason.unwrap_or_default();

    match status {
        "DONE" if is_summary => (
            "🧠 調研完成",
            Color32::from_rgb(120, 210, 255),
            Color32::from_rgb(22, 48, 68),
        ),
        "DONE" if reason.contains("status=DryRunDone") => (
            "🧪 Dry-run",
            Color32::from_rgb(255, 200, 60),
            Color32::from_rgb(68, 52, 18),
        ),
        "DONE" if reason.contains("status=Verified") || reason.contains("verified=true") => (
            "✅ 已驗證",
            Color32::from_rgb(100, 220, 100),
            Color32::from_rgb(20, 56, 26),
        ),
        "DONE" => (
            "✅ 完成",
            Color32::from_rgb(100, 220, 100),
            Color32::from_rgb(22, 48, 24),
        ),
        "PENDING" | "RUNNING" => (
            "⏳ 進行中",
            Color32::from_rgb(255, 200, 60),
            Color32::from_rgb(70, 54, 18),
        ),
        "FOLLOWING" => (
            "🔄 跟進中",
            Color32::from_rgb(120, 180, 255),
            Color32::from_rgb(20, 46, 72),
        ),
        "FOLLOWUP_NEEDED" => (
            "⚠️ 需跟進",
            Color32::from_rgb(255, 160, 80),
            Color32::from_rgb(78, 42, 20),
        ),
        "ROLLBACK" => (
            "↩ 已回滾",
            Color32::from_rgb(190, 150, 255),
            Color32::from_rgb(48, 30, 74),
        ),
        "FAILED" | "ERROR" => (
            "❌ 錯誤",
            Color32::from_rgb(220, 80, 80),
            Color32::from_rgb(72, 24, 24),
        ),
        _ => ("• 未知", Color32::GRAY, Color32::from_rgb(45, 45, 45)),
    }
}

fn build_console_log_bundle(
    console: &AgentConsoleState,
    messages: &[ChatMessage],
    log_lines: usize,
) -> String {
    format!(
        "=== Agent Console ===\n{}\n\n=== Chat History ===\n{}\n\n=== Recent Logs ===\n{}",
        console.snapshot_text(),
        chat_history_snapshot(messages),
        log_buffer::snapshot_text(log_lines)
    )
}

#[derive(Clone)]
struct ChatUiUpdate {
    reply: String,
    tools: Vec<String>,
    trace: Vec<String>,
    /// If true, this is a streaming partial — update the last assistant bubble
    /// in-place instead of pushing a new message.
    partial: bool,
    /// If Some, apply planner result to agent_console before the reply arrives.
    plan: Option<ChatPlanUpdate>,
}

#[derive(Clone)]
struct ChatPlanUpdate {
    route: String,
    intent_family: String,
    summary: String,
    steps: Vec<String>,
    recommended_skills: Vec<String>,
}

#[derive(Clone)]
struct CodingUiUpdate {
    /// Final coding agent response (None while still running).
    response: Option<crate::agents::coding_agent::CodingAgentResponse>,
    /// Partial status message shown while the agent is running.
    status_msg: String,
}

// ── Per-agent UI scratch buffers ──────────────────────────────────────────────

#[derive(Default)]
struct AgentUiScratch {
    new_objective: String,
}

// ── App state ─────────────────────────────────────────────────────────────────

pub struct SirinApp {
    tracker: TaskTracker,
    tg_auth: TelegramAuthState,
    rt: Handle,
    tab: Tab,

    // Tasks tab
    tasks: Vec<TaskEntry>,

    // Research tab
    research_tasks: Vec<ResearchTask>,
    research_topic: String,
    research_url: String,
    research_msg: String,
    pending_objectives: Option<Vec<String>>,

    // Telegram tab
    tg_code: String,
    tg_password: String,
    tg_msg: String,

    // Chat tab
    chat_messages: Vec<ChatMessage>,
    chat_input: String,
    chat_pending: bool,
    agent_console: AgentConsoleState,
    chat_tx: std::sync::mpsc::SyncSender<ChatUiUpdate>,
    chat_rx: std::sync::mpsc::Receiver<ChatUiUpdate>,

    // Coding Console
    coding_console: CodingConsoleState,
    coding_tx: std::sync::mpsc::SyncSender<CodingUiUpdate>,
    coding_rx: std::sync::mpsc::Receiver<CodingUiUpdate>,
    /// When true, show a confirmation dialog before running the coding agent
    /// (because auto_approve_writes = false in persona config).
    pending_coding_confirmation: Option<String>,
    /// Cached value of `persona.coding_agent.auto_approve_writes`.
    /// Refreshed from disk on every `refresh()` call (every 5 s) and on toggle.
    auto_approve_writes: bool,

    last_refresh: std::time::Instant,

    /// Subscriber for the process-wide agent event bus.  Drained every frame.
    event_rx: broadcast::Receiver<AgentEvent>,

    // ── UI state: new ─────────────────────────────────────────────────────────
    /// Whether the Agent / Coding debug panel is expanded in Chat tab.
    debug_panel_open: bool,
    /// Filter applied in the Tasks (Activity Log) tab.
    task_filter: TaskFilter,
    /// Set of research task IDs whose full report is expanded.
    research_expanded: std::collections::HashSet<String>,
    /// When the last coding task completed (for auto-dismissing the mini bar).
    coding_done_at: Option<std::time::Instant>,
    /// When the research startup message was set (auto-clears after 4 s).
    research_msg_at: Option<std::time::Instant>,
    /// Cached storage usage snapshot (refreshed every 5 s alongside other data).
    storage: crate::memory::StorageUsage,

    // Settings tab (multi-agent)
    /// Working copy of agents file being edited (loaded lazily).
    settings_agents: Option<crate::agent_config::AgentsFile>,
    /// Per-agent Telegram auth states (agent_id → TelegramAuthState).
    /// Populated at startup from agents.yaml; order matches agents list.
    agent_auth_states: Vec<(String, crate::telegram_auth::TelegramAuthState)>,
    /// Status message shown after save / error.
    settings_msg: String,
    settings_msg_at: Option<std::time::Instant>,
    /// Per-agent scratch buffers for inline "add objective / add command" inputs.
    /// Index matches `settings_agents.agents[i]`.
    settings_agent_scratch: Vec<AgentUiScratch>,
    /// Input buffers for the "new agent" quick-add row.
    settings_new_agent_id: String,
    settings_new_agent_name: String,
    /// Index of the currently selected agent in the left sidebar.  None = System panel.
    settings_selected_agent: Option<usize>,
    /// Active tab index in the right panel (0=身分, 1=風格, 2=目標, 3=通訊, 4=能力).
    settings_active_tab: usize,

    // ── Dispatch tab ──────────────────────────────────────────────────────────
    /// Currently selected agent id for dispatch ("" = first agent).
    dispatch_target_agent: String,
    /// Task type to dispatch.
    #[allow(dead_code)]
    dispatch_task_type: DispatchTaskType,
    /// Text input buffer for dispatch.
    dispatch_task_input: String,
    /// Feedback message shown after dispatching.
    dispatch_msg: String,
    dispatch_msg_at: Option<std::time::Instant>,
    // Per-agent filter for the Tasks tab ("" = all agents).
    task_agent_filter: String,

    // ── Dispatch detail panel ─────────────────────────────────────────────────
    /// Fleet grid selected agent index (None = none).
    dispatch_selected_agent: Option<usize>,
    /// Active tab in the detail panel: 0=活動, 1=記憶, 2=KPI, 3=待確認.
    dispatch_detail_tab: usize,
    /// Peer name/ID for manual dispatch operations.
    dispatch_manual_peer: String,
    /// Pending replies cached for the currently selected agent.
    pending_replies: Vec<crate::pending_reply::PendingReply>,
    /// Agent ID for which pending_replies was last loaded.
    pending_replies_loaded_for: String,
    /// Editable draft text per pending reply ID.
    pending_draft_edits: std::collections::HashMap<String, String>,
    /// KPI values cached in memory: agent_id → metric_key → value_str.
    kpi_values: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
}

impl SirinApp {
    /// Load a CJK-capable font from the Windows system font directory.
    /// Falls back silently if the font file cannot be read.
    pub fn setup_fonts(ctx: &egui::Context) {
        let font_path = std::path::Path::new("C:/Windows/Fonts/msjh.ttc"); // Microsoft JhengHei (繁中)
        let fallback = std::path::Path::new("C:/Windows/Fonts/msyh.ttc"); // Microsoft YaHei (簡中)

        let font_data = if font_path.exists() {
            std::fs::read(font_path).ok()
        } else if fallback.exists() {
            std::fs::read(fallback).ok()
        } else {
            None
        };

        if let Some(bytes) = font_data {
            let mut fonts = FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".to_owned(), FontData::from_owned(bytes).into());
            // Put CJK after the built-in proportional font so ASCII stays crisp.
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .push("cjk".to_owned());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            ctx.set_fonts(fonts);
        } else {
            eprintln!("[ui] Warning: no CJK font found in C:/Windows/Fonts — Chinese text may appear as boxes");
        }
    }

    pub fn new(
        tracker: TaskTracker,
        tg_auth: TelegramAuthState,
        rt: Handle,
        agent_auth_states: Vec<(String, crate::telegram_auth::TelegramAuthState)>,
    ) -> Self {
        let _ = ensure_codebase_index();

        let (chat_tx, chat_rx) = std::sync::mpsc::sync_channel(8);
        let (coding_tx, coding_rx) = std::sync::mpsc::sync_channel(4);
        let mut app = Self {
            tracker,
            tg_auth,
            rt,
            tab: Tab::Dispatch,
            tasks: Vec::new(),
            research_tasks: Vec::new(),
            research_topic: String::new(),
            research_url: String::new(),
            research_msg: String::new(),
            pending_objectives: None,
            tg_code: String::new(),
            tg_password: String::new(),
            tg_msg: String::new(),
            chat_messages: Vec::new(),
            chat_input: String::new(),
            chat_pending: false,
            agent_console: AgentConsoleState::default(),
            chat_tx,
            chat_rx,
            coding_console: CodingConsoleState::default(),
            coding_tx,
            coding_rx,
            pending_coding_confirmation: None,
            auto_approve_writes: crate::persona::Persona::load()
                .map(|p| p.coding_agent.auto_approve_writes)
                .unwrap_or(false),
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            event_rx: crate::events::subscribe(),
            debug_panel_open: false,
            task_filter: TaskFilter::All,
            research_expanded: std::collections::HashSet::new(),
            coding_done_at: None,
            research_msg_at: None,
            storage: crate::memory::StorageUsage::default(),
            settings_agents: None,
            settings_msg: String::new(),
            settings_msg_at: None,
            settings_agent_scratch: Vec::new(),
            settings_new_agent_id: String::new(),
            settings_new_agent_name: String::new(),
            settings_selected_agent: Some(0),
            settings_active_tab: 0,
            agent_auth_states,
            dispatch_target_agent: String::new(),
            dispatch_task_type: DispatchTaskType::default(),
            dispatch_task_input: String::new(),
            dispatch_msg: String::new(),
            dispatch_msg_at: None,
            task_agent_filter: String::new(),
            dispatch_selected_agent: None,
            dispatch_detail_tab: 0,
            dispatch_manual_peer: String::new(),
            pending_replies: Vec::new(),
            pending_replies_loaded_for: String::new(),
            pending_draft_edits: std::collections::HashMap::new(),
            kpi_values: std::collections::HashMap::new(),
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        match self.tracker.read_last_n(200) {
            Ok(entries) => {
                self.tasks = entries
                    .into_iter()
                    .filter(|e| e.event != "heartbeat")
                    .rev()
                    .collect();

                if let Some(summary_entry) = self
                    .tasks
                    .iter()
                    .find(|task| task.event == "research_summary_ready")
                {
                    self.agent_console.latest_task_summary = summary_entry
                        .reason
                        .as_deref()
                        .unwrap_or_default()
                        .chars()
                        .take(220)
                        .collect();
                }

                self.agent_console.ai_details = self
                    .tasks
                    .iter()
                    .find(|task| task.event.starts_with("adk:") && task.event.ends_with(":start"))
                    .map(|task| extract_ai_details(task.reason.as_deref().unwrap_or_default()))
                    .filter(|details| !details.is_empty())
                    .unwrap_or_else(|| crate::llm::shared_llm().task_log_summary());
            }
            Err(e) => eprintln!("[ui] load tasks: {e}"),
        }
        match researcher::list_research() {
            Ok(mut tasks) => {
                tasks.reverse();
                self.research_tasks = tasks;
            }
            Err(e) => eprintln!("[ui] load research: {e}"),
        }
        // Pick up any pending persona objective proposal from the researcher.
        if let Some(proposed) = researcher::take_pending_objectives() {
            self.pending_objectives = Some(proposed);
        }
        // Sync the cached auto_approve_writes from persona config.
        self.auto_approve_writes = crate::persona::Persona::load()
            .map(|p| p.coding_agent.auto_approve_writes)
            .unwrap_or(false);
        self.storage = crate::memory::storage_usage();
        self.last_refresh = std::time::Instant::now();
    }
}

// ── eframe App impl ───────────────────────────────────────────────────────────

impl eframe::App for SirinApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Intercept close → minimize to background instead of quitting.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        // ── Timed auto-dismiss: coding mini bar (8 s after completion) ──────────
        if let Some(done_at) = self.coding_done_at {
            if done_at.elapsed() > std::time::Duration::from_secs(8) {
                self.coding_console = CodingConsoleState::default();
                self.coding_done_at = None;
            }
        }

        // ── Timed auto-dismiss: research startup message (4 s) ───────────────
        if let Some(msg_at) = self.research_msg_at {
            if msg_at.elapsed() > std::time::Duration::from_secs(4) {
                self.research_msg.clear();
                self.research_msg_at = None;
            }
        }

        // ── Timed auto-dismiss: settings save message (4 s) ─────────────────
        if let Some(msg_at) = self.settings_msg_at {
            if msg_at.elapsed() > std::time::Duration::from_secs(4) {
                self.settings_msg.clear();
                self.settings_msg_at = None;
            }
        }

        // ── Timed auto-dismiss: dispatch feedback message (4 s) ──────────────
        if let Some(msg_at) = self.dispatch_msg_at {
            if msg_at.elapsed() > std::time::Duration::from_secs(4) {
                self.dispatch_msg.clear();
                self.dispatch_msg_at = None;
            }
        }

        // Poll for LLM reply from background chat task.
        if self.chat_pending {
            // Drain all pending updates this frame (may receive multiple partials).
            let mut got_final = false;
            while let Ok(update) = self.chat_rx.try_recv() {
                // Apply plan update whenever it arrives (first partial or final).
                if let Some(plan) = update.plan {
                    self.agent_console.route = plan.route;
                    self.agent_console.intent_family = plan.intent_family;
                    self.agent_console.summary = plan.summary;
                    self.agent_console.steps = plan.steps;
                    self.agent_console.recommended_skills = plan.recommended_skills;
                    self.agent_console.status = "Executing…".to_string();
                }

                // Update the chat bubble (partial: in-place; final: complete).
                let bubble_text = update.reply.clone();
                if !bubble_text.is_empty() {
                    if let Some(last) = self.chat_messages.last_mut() {
                        if last.role == ChatRole::Assistant {
                            last.text = bubble_text;
                        } else {
                            self.chat_messages.push(ChatMessage {
                                role: ChatRole::Assistant,
                                text: bubble_text,
                            });
                        }
                    } else {
                        self.chat_messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            text: bubble_text,
                        });
                    }
                }

                if !update.partial {
                    self.agent_console.tools = update.tools;
                    self.agent_console.trace = update
                        .trace
                        .into_iter()
                        .rev()
                        .take(6)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    self.agent_console.status = "Idle".to_string();
                    got_final = true;
                }
            }
            if got_final {
                self.chat_pending = false;
            } else {
                // Keep repainting frequently while streaming.
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        // Poll for coding agent result from background task.
        while let Ok(update) = self.coding_rx.try_recv() {
            self.coding_console.status = update.status_msg.clone();
            if let Some(resp) = update.response {
                self.coding_console.outcome = resp.outcome.clone();
                self.coding_console.change_summary = resp.change_summary.clone();
                self.coding_console.files_modified = resp.files_modified.clone();
                self.coding_console.trace = resp.trace.clone();
                self.coding_console.diff = resp.diff.clone();
                self.coding_console.verified = resp.verified;
                self.coding_console.verification_output = resp.verification_output.clone();
                self.coding_console.dry_run = resp.dry_run;
                self.coding_console.status = coding_status_text(&resp);
                self.agent_console.status = self.coding_console.status.clone();
                if !resp.change_summary.is_empty() {
                    self.agent_console.summary = resp.change_summary.clone();
                }
                // Start the auto-dismiss timer for the mini status bar.
                self.coding_done_at = Some(std::time::Instant::now());
                // Push the outcome as an assistant message in the chat.
                let summary = format!(
                    "**[Coding Agent]** {}\n\n{}{}{}",
                    resp.outcome,
                    if resp.change_summary.is_empty() {
                        String::new()
                    } else {
                        format!("🧾 變更摘要：{}\n", resp.change_summary)
                    },
                    if resp.files_modified.is_empty() {
                        String::new()
                    } else {
                        format!("📁 已修改：{}\n", resp.files_modified.join(", "))
                    },
                    if resp.dry_run {
                        "（Dry-run 模式：檔案未寫入）"
                    } else {
                        ""
                    }
                );
                if let Some(last) = self.chat_messages.last_mut() {
                    if last.role == ChatRole::Assistant {
                        last.text = summary;
                    } else {
                        self.chat_messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            text: summary,
                        });
                    }
                } else {
                    self.chat_messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        text: summary,
                    });
                }
                self.chat_pending = false;
            }
            ctx.request_repaint();
        }

        // Auto-refresh every 5 s.
        if self.last_refresh.elapsed() > std::time::Duration::from_secs(5) {
            self.refresh();
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(5));

        // Drain agent-event-bus messages (non-blocking).
        loop {
            match self.event_rx.try_recv() {
                Ok(AgentEvent::ResearchRequested { topic, url }) => {
                    // Auto-fill the Research tab form and kick off the research run.
                    self.tab = Tab::Research;
                    self.research_topic = topic.clone();
                    if let Some(ref u) = url {
                        self.research_url = u.clone();
                    }
                    let rt = self.rt.clone();
                    rt.spawn(async move {
                        let task =
                            crate::agents::research_agent::run_research_via_adk(topic, url).await;
                        eprintln!("[ui] auto-research '{}' → {:?}", task.id, task.status);
                    });
                    self.research_msg = format!("自動啟動調研：{}", self.research_topic.trim());
                    self.research_msg_at = Some(std::time::Instant::now());
                    self.research_topic.clear();
                    self.research_url.clear();
                }
                Ok(AgentEvent::PersonaUpdated { new_objectives }) => {
                    // Mirror what the Research tab's pending_objectives gate does,
                    // but triggered by event instead of polling.
                    if self.pending_objectives.is_none() {
                        self.pending_objectives = Some(new_objectives);
                    }
                }
                Ok(AgentEvent::CodingAgentCompleted { task, success, files_modified }) => {
                    // Refresh task list immediately so the result shows without waiting 5 s.
                    self.refresh();
                    // Also update the coding console status bar so the Chat tab shows it.
                    if self.coding_console.task.is_empty() || self.coding_console.task == task {
                        let status = if success { "✅ 完成（事件觸發）" } else { "❌ 失敗（事件觸發）" };
                        self.coding_console.status = status.to_string();
                        self.coding_console.task = task;
                        self.coding_console.files_modified = files_modified;
                        if success {
                            self.coding_done_at = Some(std::time::Instant::now());
                        }
                    }
                }
                Ok(AgentEvent::ReplyPendingApproval { agent_id, .. }) => {
                    // Reload pending replies if the currently selected agent matches.
                    let sel_id = self.dispatch_selected_agent
                        .and_then(|i| self.settings_agents.as_ref()?.agents.get(i))
                        .map(|a| a.id.clone());
                    if sel_id.as_deref() == Some(agent_id.as_str()) {
                        self.pending_replies = crate::pending_reply::load_pending(&agent_id);
                        self.pending_replies_loaded_for = agent_id.clone();
                    }
                    // Switch focus to dispatch → 待確認 tab.
                    self.tab = Tab::Dispatch;
                    self.dispatch_detail_tab = 3;
                }
                Ok(_) => {} // other events (ResearchCompleted, FollowupTriggered, ChatAgentReplied)
                Err(broadcast::error::TryRecvError::Lagged(_)) => {} // skip lagged events
                Err(_) => break, // Empty or Closed
            }
        }

        // ── Top panel (tabs + refresh) ────────────────────────────────────────
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Sirin");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Dispatch, "🗂 調度台");
                ui.selectable_value(&mut self.tab, Tab::Tasks, "📋 活動記錄");

                // Research tab: badge when a task is running.
                let research_running = self
                    .research_tasks
                    .iter()
                    .any(|t| t.status == ResearchStatus::Running);
                let research_label = if research_running {
                    "🔬 調研 ●"
                } else {
                    "🔬 調研"
                };
                let res_tab = ui.selectable_value(&mut self.tab, Tab::Research, research_label);
                if research_running {
                    res_tab.on_hover_text("有調研任務進行中");
                }

                // Chat tab: badge when pending coding confirmation.
                let chat_label = if self.pending_coding_confirmation.is_some() {
                    "💬 對話 ⚠"
                } else {
                    "💬 對話"
                };
                ui.selectable_value(&mut self.tab, Tab::Chat, chat_label);

                // Settings tab: show Telegram connection badge.
                let tg_connected = matches!(self.tg_auth.status(), TelegramStatus::Connected);
                let tg_needs_auth = matches!(
                    self.tg_auth.status(),
                    TelegramStatus::CodeRequired | TelegramStatus::PasswordRequired { .. }
                );
                let settings_label = if tg_needs_auth {
                    "⚙ 設定 ⚠"
                } else if tg_connected {
                    "⚙ 設定 ✈●"
                } else {
                    "⚙ 設定"
                };
                let settings_tab = ui.selectable_value(&mut self.tab, Tab::Settings, settings_label);
                if tg_connected {
                    settings_tab.on_hover_text("Telegram 已連線");
                } else if tg_needs_auth {
                    settings_tab.on_hover_text("Telegram 需要認證");
                }
                // Log tab: badge when recent errors exist.
                let has_errors = log_buffer::recent(50)
                    .iter()
                    .any(|l| l.contains("[ERROR]") || l.contains("error") || l.contains("failed"));
                let log_label = if has_errors { "📋 Log ●" } else { "📋 Log" };
                let log_tab = ui.selectable_value(&mut self.tab, Tab::Log, log_label);
                if has_errors {
                    log_tab.on_hover_text("Log 中有錯誤訊息");
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳").on_hover_text("立即刷新").clicked() {
                        self.refresh();
                    }
                    let secs = self.last_refresh.elapsed().as_secs();
                    ui.small(format!("{secs}s 前"));
                });
            });
        });

        // ── Central panel ─────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Dispatch => self.show_dispatch(ui),
            Tab::Tasks => self.show_tasks(ui),
            Tab::Research => self.show_research(ui),
            Tab::Chat => self.show_chat(ui),
            Tab::Settings => self.show_settings(ui),
            Tab::Log => self.show_log(ui),
        });
    }
}

// ── Tab rendering ─────────────────────────────────────────────────────────────

impl SirinApp {
    fn show_tasks(&mut self, ui: &mut egui::Ui) {
        // ── Storage usage panel ───────────────────────────────────────────────
        use crate::memory::StorageUsage;
        let s = &self.storage;
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(22, 28, 38))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("💾 儲存空間").strong());
                    ui.separator();
                    ui.colored_label(
                        Color32::from_rgb(130, 200, 255),
                        format!("總計 {}", StorageUsage::fmt_bytes(s.total_bytes)),
                    );
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    let items = [
                        ("記憶 DB", s.memory_db_bytes),
                        ("Call Graph", s.call_graph_bytes),
                        ("調研記錄", s.research_log_bytes),
                        ("任務記錄", s.task_log_bytes),
                        ("對話 Context", s.context_bytes),
                    ];
                    for (label, bytes) in items {
                        if bytes > 0 {
                            egui::Frame::new()
                                .fill(Color32::from_rgb(32, 40, 55))
                                .inner_margin(egui::Margin::symmetric(8, 3))
                                .corner_radius(4.0)
                                .show(ui, |ui| {
                                    ui.small(format!(
                                        "{label}  {}",
                                        StorageUsage::fmt_bytes(bytes)
                                    ));
                                });
                        }
                    }
                    if s.total_bytes == 0 {
                        ui.colored_label(Color32::GRAY, "尚無資料");
                    }
                });
            });

        ui.add_space(4.0);

        // ── Filter bar ────────────────────────────────────────────────────────
        let running_count = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status.as_deref(), Some("PENDING") | Some("RUNNING") | Some("FOLLOWING")))
            .count();
        let attention_count = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status.as_deref(), Some("FOLLOWUP_NEEDED") | Some("FAILED") | Some("ERROR") | Some("ROLLBACK")))
            .count();
        let done_count = self
            .tasks
            .iter()
            .filter(|t| t.status.as_deref() == Some("DONE"))
            .count();

        // Build agent name list for filter dropdown (lazy from settings_agents or task log).
        let agent_names: Vec<String> = {
            use crate::agent_config::AgentsFile;
            if self.settings_agents.is_none() {
                self.settings_agents = AgentsFile::load().ok().or_else(|| Some(AgentsFile::default()));
            }
            let mut names: Vec<String> = self.settings_agents.as_ref()
                .map(|f| f.agents.iter().map(|a| a.identity.name.clone()).collect())
                .unwrap_or_default();
            // Also include names seen in task log but not in agents config.
            for t in &self.tasks {
                if !names.contains(&t.persona) {
                    names.push(t.persona.clone());
                }
            }
            names.dedup();
            names
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new("活動記錄").strong());
            ui.separator();
            ui.selectable_value(&mut self.task_filter, TaskFilter::All, "全部");
            ui.selectable_value(&mut self.task_filter, TaskFilter::Running, "進行中");
            ui.selectable_value(&mut self.task_filter, TaskFilter::Done, "完成");
            ui.selectable_value(&mut self.task_filter, TaskFilter::Failed, "需處理");
            ui.separator();
            // Agent filter dropdown.
            let agent_label = if self.task_agent_filter.is_empty() {
                "全部 Agent".to_string()
            } else {
                self.task_agent_filter.clone()
            };
            egui::ComboBox::from_id_salt("task_agent_filter")
                .selected_text(RichText::new(&agent_label).small())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.task_agent_filter, String::new(), "全部 Agent");
                    for name in &agent_names {
                        ui.selectable_value(&mut self.task_agent_filter, name.clone(), name.as_str());
                    }
                });
            ui.separator();
            ui.small(format!("⏳ {}", running_count));
            ui.small(format!("⚠️ {}", attention_count));
            ui.small(format!("✅ {}", done_count));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let total = self.tasks.len();
                ui.small(format!("{total} 筆"));
                if ui
                    .button(RichText::new("🗑 清除").small().color(Color32::from_rgb(200, 80, 80)))
                    .on_hover_text("清除所有活動紀錄（檔案將被截空）")
                    .clicked()
                {
                    if let Err(e) = self.tracker.clear() {
                        eprintln!("[ui] clear task log: {e}");
                    }
                    self.tasks.clear();
                }
            });
        });
        ui.separator();

        let filter = self.task_filter;
        let agent_filter = self.task_agent_filter.clone();
        let row_height = 18.0;
        let col_widths = [120.0_f32, 230.0, 50.0];

        // Column headers
        ui.horizontal(|ui| {
            ui.add_sized(
                [col_widths[0], row_height],
                egui::Label::new(RichText::new("時間").strong().small()),
            );
            ui.add_sized(
                [col_widths[1], row_height],
                egui::Label::new(RichText::new("事件").strong().small()),
            );
            ui.label(RichText::new("狀態").strong().small());
        });
        ui.separator();

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for task in &self.tasks {
                let status = task.status.as_deref().unwrap_or("—");
                // Apply status filter.
                let passes = match filter {
                    TaskFilter::All => true,
                    TaskFilter::Running => matches!(status, "PENDING" | "RUNNING" | "FOLLOWING"),
                    TaskFilter::Done => status == "DONE",
                    TaskFilter::Failed => {
                        matches!(status, "FAILED" | "ERROR" | "FOLLOWUP_NEEDED" | "ROLLBACK")
                    }
                };
                // Apply agent filter.
                let agent_passes = agent_filter.is_empty() || task.persona == agent_filter;
                if !passes || !agent_passes {
                    continue;
                }

                let ts = task.timestamp.get(..19).unwrap_or(&task.timestamp);
                let is_summary = task.event == "research_summary_ready";
                let (badge_text, badge_fg, badge_bg) =
                    task_status_badge(status, task.reason.as_deref(), is_summary);

                ui.horizontal(|ui| {
                    ui.add_sized(
                        [col_widths[0], row_height],
                        egui::Label::new(RichText::new(ts).monospace().small()),
                    );
                    let preview = task
                        .message_preview
                        .as_deref()
                        .or(task.reason.as_deref())
                        .unwrap_or(&task.event);
                    let event_label = if is_summary {
                        "🧠 research_summary"
                    } else if task.event == "adk_coding_fail_fast" {
                        "⚠ coding_fail_fast"
                    } else if task.event == "adk_coding_rollback" {
                        "↩ coding_rollback"
                    } else if task.event == "adk_coding_agent_done" {
                        "⚙ coding_done"
                    } else {
                        &task.event
                    };
                    let label_text = format!(
                        "{} — {}",
                        event_label,
                        preview.chars().take(80).collect::<String>()
                    );
                    ui.add_sized(
                        [col_widths[1], row_height],
                        egui::Label::new(&label_text).truncate(),
                    );
                    egui::Frame::new()
                        .fill(badge_bg)
                        .stroke(egui::Stroke::new(1.0, badge_fg))
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .corner_radius(6.0)
                        .show(ui, |ui| {
                            ui.label(RichText::new(badge_text).small().strong().color(badge_fg));
                        });
                });

                // Inline summary expansion for research / fail-fast events.
                if is_summary || matches!(status, "FOLLOWUP_NEEDED" | "ROLLBACK" | "FAILED" | "ERROR") {
                    if let Some(reason) = task.reason.as_deref() {
                        let preview: String = reason.chars().take(180).collect();
                        ui.add_space(1.0);
                        ui.colored_label(badge_fg, format!("   ↳ {preview}"));
                    }
                }
            }
        });
    }

    fn show_research(&mut self, ui: &mut egui::Ui) {
        // ── Persona safety gate ────────────────────────────────────────────────
        if let Some(proposed) = self.pending_objectives.clone() {
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(40, 35, 15))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("⚠ AI 提議更新 Persona 目標（需您確認）")
                            .color(Color32::YELLOW)
                            .strong(),
                    );
                    for (i, obj) in proposed.iter().enumerate() {
                        ui.label(format!("  {}. {}", i + 1, obj));
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .button(
                                RichText::new("✅ 套用").color(Color32::from_rgb(100, 220, 100)),
                            )
                            .clicked()
                        {
                            match crate::persona::Persona::load() {
                                Ok(mut p) => {
                                    p.objectives = proposed.clone();
                                    match p.save() {
                                        Ok(()) => {
                                            self.research_msg = "Persona 目標已更新".to_string();
                                        }
                                        Err(e) => {
                                            self.research_msg = format!("儲存失敗: {e}");
                                        }
                                    }
                                }
                                Err(e) => {
                                    self.research_msg = format!("載入 Persona 失敗: {e}");
                                }
                            }
                            self.pending_objectives = None;
                        }
                        if ui
                            .button(RichText::new("❌ 拒絕").color(Color32::from_rgb(220, 80, 80)))
                            .clicked()
                        {
                            self.pending_objectives = None;
                            self.research_msg = "已拒絕 AI 目標提議".to_string();
                        }
                    });
                });
            ui.separator();
        }

        // ── New research form ──────────────────────────────────────────────────
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("新調研任務").strong());
            ui.horizontal(|ui| {
                ui.label("主題：");
                ui.text_edit_singleline(&mut self.research_topic);
            });
            ui.horizontal(|ui| {
                ui.label("URL：");
                ui.text_edit_singleline(&mut self.research_url);
                ui.small("（選填）");
            });
            ui.horizontal(|ui| {
                let can_start = !self.research_topic.trim().is_empty();
                if ui
                    .add_enabled(can_start, egui::Button::new("▶ 開始調研"))
                    .clicked()
                {
                    let topic = self.research_topic.trim().to_string();
                    let url = if self.research_url.trim().is_empty() {
                        None
                    } else {
                        Some(self.research_url.trim().to_string())
                    };
                    let rt = self.rt.clone();
                    rt.spawn(async move {
                        let task =
                            crate::agents::research_agent::run_research_via_adk(topic, url).await;
                        eprintln!("[ui] research '{}' → {:?}", task.id, task.status);
                    });
                    self.research_msg = format!("已啟動：{}", self.research_topic.trim());
                    self.research_msg_at = Some(std::time::Instant::now());
                    self.research_topic.clear();
                    self.research_url.clear();
                }
                if !self.research_msg.is_empty() {
                    ui.colored_label(Color32::from_rgb(100, 220, 100), &self.research_msg);
                }
            });
        });

        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!("{} 筆調研記錄", self.research_tasks.len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(RichText::new("🗑 清除").small().color(Color32::from_rgb(200, 80, 80)))
                    .on_hover_text("清除所有調研紀錄（檔案將被截空）")
                    .clicked()
                {
                    if let Err(e) = researcher::clear_research() {
                        eprintln!("[ui] clear research: {e}");
                    }
                    self.research_tasks.clear();
                    self.research_expanded.clear();
                }
            });
        });

        // Collect IDs to expand/collapse without borrowing self during the loop.
        let mut toggle_expand: Option<String> = None;

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for task in &self.research_tasks {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    // ── Header row ────────────────────────────────────────────
                    ui.horizontal(|ui| {
                        let (color, label) = match task.status {
                            ResearchStatus::Done => (Color32::from_rgb(100, 220, 100), "完成"),
                            ResearchStatus::Running => (Color32::YELLOW, "進行中"),
                            ResearchStatus::Failed => (Color32::from_rgb(220, 80, 80), "失敗"),
                        };
                        ui.colored_label(color, label);
                        ui.strong(&task.topic);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.small(task.started_at.get(..10).unwrap_or(&task.started_at));
                            if let Some(ref url) = task.url {
                                ui.small(url.chars().take(40).collect::<String>());
                            }
                        });
                    });

                    // ── Phase progress (Running) ───────────────────────────────
                    if task.status == ResearchStatus::Running {
                        let phases = ["fetch", "overview", "questions", "search", "synthesis"];
                        // Determine current phase from completed steps.
                        let completed_phases: Vec<&str> =
                            task.steps.iter().map(|s| s.phase.as_str()).collect();
                        let current_idx = if completed_phases.is_empty() {
                            0
                        } else {
                            let last = *completed_phases.last().unwrap();
                            if last.starts_with("research_q") {
                                3
                            } else {
                                phases
                                    .iter()
                                    .position(|&p| p == last)
                                    .map(|i| i + 1)
                                    .unwrap_or(0)
                            }
                        };
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            for (i, phase) in phases.iter().enumerate() {
                                let is_done = i < current_idx;
                                let is_current = i == current_idx;
                                let (color, icon) = if is_done {
                                    (Color32::from_rgb(100, 220, 100), "✓")
                                } else if is_current {
                                    (Color32::YELLOW, "▶")
                                } else {
                                    (Color32::from_rgb(80, 80, 80), "·")
                                };
                                ui.colored_label(color, format!("{icon} {phase}"));
                                if i < phases.len() - 1 {
                                    ui.colored_label(Color32::from_rgb(60, 60, 60), "→");
                                }
                            }
                        });
                    }

                    // ── Report preview / expand ────────────────────────────────
                    if let Some(ref report) = task.final_report {
                        let is_expanded = self.research_expanded.contains(&task.id);
                        if is_expanded {
                            ScrollArea::vertical()
                                .id_salt(format!("report_{}", task.id))
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    ui.label(report.as_str());
                                });
                            if ui.small_button("▲ 收起報告").clicked() {
                                toggle_expand = Some(task.id.clone());
                            }
                        } else {
                            let preview: String = report.chars().take(200).collect();
                            ui.small(format!("{}…", preview.trim_end()));
                            if ui.small_button("▼ 展開完整報告").clicked() {
                                toggle_expand = Some(task.id.clone());
                            }
                        }
                    } else if task.status == ResearchStatus::Failed {
                        // Show error reason from steps.
                        if let Some(err_step) = task.steps.iter().find(|s| s.phase == "error") {
                            ui.colored_label(
                                Color32::from_rgb(220, 100, 100),
                                format!(
                                    "❌ {}",
                                    err_step.output.chars().take(120).collect::<String>()
                                ),
                            );
                        }
                    }
                });
            }
        });

        // Apply deferred toggle outside the borrow.
        if let Some(id) = toggle_expand {
            if self.research_expanded.contains(&id) {
                self.research_expanded.remove(&id);
            } else {
                self.research_expanded.insert(id);
            }
        }
    }



    fn show_chat(&mut self, ui: &mut egui::Ui) {
        // ── Top status bar ────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            let status_color = if self.chat_pending {
                Color32::YELLOW
            } else {
                Color32::from_rgb(100, 220, 100)
            };
            ui.colored_label(status_color, &self.agent_console.status);
            if !self.agent_console.route.is_empty() && self.agent_console.route != "pending…" {
                ui.separator();
                ui.small(format!("→ {}", self.agent_console.route));
            }
            if !self.agent_console.intent_family.is_empty() {
                ui.small(format!("[{}]", self.agent_console.intent_family));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("複製對話").clicked() {
                    ui.ctx()
                        .copy_text(chat_history_snapshot(&self.chat_messages));
                }
                ui.separator();
                // Debug panel toggle button.
                let debug_label = if self.debug_panel_open {
                    "🔍 隱藏診斷"
                } else {
                    "🔍 診斷"
                };
                if ui.small_button(debug_label).clicked() {
                    self.debug_panel_open = !self.debug_panel_open;
                }
            });
        });

        // ── Debug panel (Agent Console + Coding Console, hidden by default) ───
        if self.debug_panel_open {
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(20, 22, 30))
                .show(ui, |ui| {
                    ui.label(RichText::new("Agent Console").strong().small());
                    if !self.agent_console.summary.is_empty() {
                        ui.small(&self.agent_console.summary);
                    }
                    if !self.agent_console.ai_details.is_empty() {
                        ui.small(format!("🤖 {}", self.agent_console.ai_details));
                    }
                    if !self.agent_console.steps.is_empty() {
                        ui.small(format!("Steps: {}", self.agent_console.steps.join(" → ")));
                    }
                    if !self.agent_console.recommended_skills.is_empty() {
                        ui.small(format!(
                            "Skills: {}",
                            self.agent_console.recommended_skills.join(", ")
                        ));
                    }
                    if !self.agent_console.tools.is_empty() {
                        ui.small(format!("Tools: {}", self.agent_console.tools.join(", ")));
                    }
                    if !self.agent_console.trace.is_empty() {
                        ui.collapsing(
                            format!("Execution Trace ({} items)", self.agent_console.trace.len()),
                            |ui| {
                                for item in &self.agent_console.trace {
                                    ui.small(item);
                                }
                            },
                        );
                    }
                    if !self.agent_console.latest_task_summary.is_empty() {
                        ui.collapsing("Latest Research Summary", |ui| {
                            ui.small(&self.agent_console.latest_task_summary);
                        });
                    }

                    // Coding Console inside debug panel.
                    if !self.coding_console.task.is_empty() {
                        ui.separator();
                        ui.label(
                            RichText::new("⚙ Coding Console")
                                .strong()
                                .small()
                                .color(Color32::from_rgb(100, 200, 255)),
                        );
                        ui.small(format!(
                            "Task: {}",
                            self.coding_console
                                .task
                                .chars()
                                .take(100)
                                .collect::<String>()
                        ));
                        if self.coding_console.dry_run {
                            ui.colored_label(Color32::from_rgb(255, 200, 60), "⚠ Dry-run 模式");
                        }
                        if !self.coding_console.change_summary.is_empty() {
                            ui.small(format!("🧾 {}", self.coding_console.change_summary));
                        }
                        if !self.coding_console.files_modified.is_empty() {
                            ui.small(format!(
                                "📁 {}",
                                self.coding_console.files_modified.join(", ")
                            ));
                        }
                        if self.coding_console.verified {
                            ui.colored_label(
                                Color32::from_rgb(100, 220, 100),
                                "✅ cargo check passed",
                            );
                        }
                        if !self.coding_console.trace.is_empty() {
                            ui.collapsing(
                                format!("ReAct Trace ({} steps)", self.coding_console.trace.len()),
                                |ui| {
                                    ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                                        for step in &self.coding_console.trace {
                                            egui::Frame::new()
                                                .fill(Color32::from_rgb(25, 35, 45))
                                                .inner_margin(egui::Margin::symmetric(6, 4))
                                                .corner_radius(4.0)
                                                .show(ui, |ui| {
                                                    ui.small(step);
                                                });
                                            ui.add_space(2.0);
                                        }
                                    });
                                },
                            );
                        }
                        if let Some(ref diff) = self.coding_console.diff {
                            if !diff.trim().is_empty() {
                                ui.collapsing("📝 Git Diff", |ui| {
                                    ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                                        let preview: String = diff.chars().take(2000).collect();
                                        ui.add(
                                            TextEdit::multiline(&mut preview.as_str())
                                                .code_editor()
                                                .desired_rows(6)
                                                .desired_width(f32::INFINITY),
                                        );
                                    });
                                });
                            }
                        }
                        if let Some(ref vout) = self.coding_console.verification_output {
                            if !vout.trim().is_empty() {
                                ui.collapsing("🔍 Verification", |ui| {
                                    ui.small(vout.chars().take(400).collect::<String>());
                                });
                            }
                        }
                        ui.horizontal(|ui| {
                            if ui.small_button("複製 Coding Console").clicked() {
                                ui.ctx().copy_text(self.coding_console.snapshot_text());
                            }
                            if ui.small_button("複製全部 (Console+Log)").clicked() {
                                let bundle = build_console_log_bundle(
                                    &self.agent_console,
                                    &self.chat_messages,
                                    250,
                                );
                                ui.ctx().copy_text(bundle);
                            }
                        });
                    }
                });
        }

        // ── Mini coding status bar (visible outside debug panel when running) ─
        if !self.coding_console.task.is_empty() && !self.debug_panel_open {
            let is_done = self.coding_console.status.contains("完成")
                || self.coding_console.status.contains("✅");
            let is_err = self.coding_console.status.contains("錯誤")
                || self.coding_console.status.contains("Error");
            let bar_color = if is_done {
                Color32::from_rgb(100, 220, 100)
            } else if is_err {
                Color32::from_rgb(220, 80, 80)
            } else {
                Color32::YELLOW
            };
            egui::Frame::new()
                .fill(Color32::from_rgb(20, 30, 40))
                .inner_margin(egui::Margin::symmetric(8, 4))
                .corner_radius(4.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(bar_color, "⚙");
                        ui.small(format!("{}", self.coding_console.status));
                        if !self.coding_console.files_modified.is_empty() {
                            ui.small(format!(
                                "· 📁 {}",
                                self.coding_console.files_modified.join(", ")
                            ));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("詳情").clicked() {
                                self.debug_panel_open = true;
                            }
                        });
                    });
                });
        }

        // ── Coding pre-flight confirmation dialog ─────────────────────────────
        if let Some(ref pending_task) = self.pending_coding_confirmation.clone() {
            ui.add_space(4.0);
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(40, 30, 10))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("⚠ Coding Agent 需要寫入檔案權限")
                            .color(Color32::YELLOW)
                            .strong(),
                    );
                    ui.small("任務：");
                    ui.small(pending_task.chars().take(100).collect::<String>());
                    ui.small("persona.yaml 中 auto_approve_writes = false。確認後 AI 將直接寫入檔案，此操作不可逆。");
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("✅ 允許寫入").color(Color32::from_rgb(100, 220, 100)))
                            .clicked()
                        {
                            // Directly spawn the coding agent with dry_run=false.
                            let task_clone = pending_task.clone();
                            self.pending_coding_confirmation = None;
                            let coding_tx = self.coding_tx.clone();
                            let tx = self.chat_tx.clone();
                            let rt = self.rt.clone();
                            self.chat_pending = true;
                            self.coding_console = CodingConsoleState {
                                status: "執行中（允許寫入）…".to_string(),
                                task: task_clone.clone(),
                                ..Default::default()
                            };
                            self.agent_console.ai_details = crate::llm::shared_llm().task_log_summary();
                            rt.spawn(async move {
                                let _ = tx.try_send(ChatUiUpdate {
                                    reply: "⚙ Coding Agent 啟動中（允許寫入）…".to_string(),
                                    tools: vec![],
                                    trace: vec![],
                                    partial: true,
                                    plan: Some(ChatPlanUpdate {
                                        route: "coding".to_string(),
                                        intent_family: "code_modification".to_string(),
                                        summary: "Running local AI Coding Agent with write permission…".to_string(),
                                        steps: vec![
                                            "gather context".to_string(),
                                            "plan".to_string(),
                                            "ReAct loop".to_string(),
                                            "verify".to_string(),
                                        ],
                                        recommended_skills: vec!["coding_agent".to_string()],
                                    }),
                                });
                                let resp = crate::agents::coding_agent::run_coding_via_adk(
                                    task_clone, false, None, None,
                                )
                                .await;
                                let _ = coding_tx.try_send(CodingUiUpdate {
                                    response: Some(resp),
                                    status_msg: "完成".to_string(),
                                });
                            });
                        }
                        if ui
                            .button(RichText::new("👁 僅讀取（Dry-run）").color(Color32::from_rgb(100, 180, 255)))
                            .clicked()
                        {
                            // Run in dry-run mode — set the input back and force dry_run flag.
                            let task_clone = pending_task.clone();
                            self.pending_coding_confirmation = None;
                            let coding_tx = self.coding_tx.clone();
                            let rt = self.rt.clone();
                            self.chat_pending = true;
                            self.coding_console = CodingConsoleState {
                                status: "Dry-run 執行中…".to_string(),
                                task: task_clone.clone(),
                                ..Default::default()
                            };
                            self.agent_console.ai_details = crate::llm::shared_llm().task_log_summary();
                            rt.spawn(async move {
                                let resp = crate::agents::coding_agent::run_coding_via_adk(
                                    task_clone, true, None, None,
                                )
                                .await;
                                let _ = coding_tx.try_send(CodingUiUpdate {
                                    response: Some(resp),
                                    status_msg: "Dry-run 完成".to_string(),
                                });
                            });
                        }
                        if ui
                            .button(RichText::new("❌ 取消").color(Color32::from_rgb(220, 80, 80)))
                            .clicked()
                        {
                            self.pending_coding_confirmation = None;
                            // Remove the pending user message bubble.
                            if let Some(last) = self.chat_messages.last() {
                                if last.role == ChatRole::User {
                                    self.chat_messages.pop();
                                }
                            }
                        }
                    });
                });
        }

        ui.add_space(8.0);

        // ── Message history ───────────────────────────────────────────────────
        let available = ui.available_height();
        let input_area_height = 120.0;

        let chosen_example = ScrollArea::vertical()
            .max_height(available - input_area_height)
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                let mut chosen: Option<String> = None;
                if self.chat_messages.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(30.0);
                        ui.colored_label(
                            Color32::from_rgb(160, 160, 160),
                            RichText::new("Sirin").size(20.0).strong(),
                        );
                        ui.add_space(4.0);
                        ui.colored_label(Color32::GRAY, "本地 AI Agent · 對話 / 編程 / 調研");
                        ui.add_space(20.0);
                        ui.colored_label(Color32::from_rgb(100, 100, 100), "試試這些：");
                        ui.add_space(6.0);
                        let examples = [
                            ("📋", "這個專案的架構是什麼？"),
                            ("⚙", "幫我找出 researcher.rs 裡的 TODO"),
                            ("🔬", "研究 Rust async/await 的底層原理"),
                            ("💬", "你能做什麼？"),
                        ];
                        for (icon, prompt) in &examples {
                            let resp = egui::Frame::new()
                                .fill(Color32::from_rgb(35, 40, 50))
                                .inner_margin(egui::Margin::symmetric(10, 5))
                                .corner_radius(8.0)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(format!("{icon} {prompt}"))
                                                .color(Color32::from_rgb(180, 190, 210)),
                                        )
                                        .sense(egui::Sense::click()),
                                    )
                                });
                            if resp.inner.clicked() {
                                chosen = Some(prompt.to_string());
                            }
                            ui.add_space(3.0);
                        }
                    });
                }

                for msg in &self.chat_messages {
                    let (bg, label, text_color) = match msg.role {
                        ChatRole::User => (Color32::from_rgb(40, 60, 100), "你", Color32::WHITE),
                        ChatRole::Assistant => (
                            Color32::from_rgb(45, 55, 45),
                            "Sirin",
                            Color32::from_rgb(200, 240, 200),
                        ),
                    };

                    egui::Frame::new()
                        .fill(bg)
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .corner_radius(6.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.colored_label(Color32::GRAY, label);
                            });
                            ui.colored_label(text_color, &msg.text);
                        });
                    ui.add_space(4.0);
                }

                if self.chat_pending {
                    // Animate the thinking dots using elapsed time.
                    let dot_count = ((ui.ctx().input(|i| i.time) * 2.0) as usize % 4) + 1;
                    let dots: String = "●".repeat(dot_count) + &"○".repeat(4 - dot_count);
                    egui::Frame::new()
                        .fill(Color32::from_rgb(45, 55, 45))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .corner_radius(6.0)
                        .show(ui, |ui| {
                            ui.colored_label(Color32::GRAY, "Sirin");
                            ui.colored_label(Color32::YELLOW, format!("思考中  {dots}"));
                        });
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(500));
                }
                chosen
            })
            .inner;

        // Apply chosen example prompt (fills the input and immediately submits).
        if let Some(prompt) = chosen_example {
            self.chat_input = prompt;
        }

        ui.separator();

        // ── Input area ────────────────────────────────────────────────────────
        // Row 1: text input (full width)
        let input = ui.add(
            TextEdit::multiline(&mut self.chat_input)
                .hint_text("輸入訊息…（Enter 送出，Shift+Enter 換行）")
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );

        // Row 2: action buttons
        // Snapshot the cached auto_approve flag so the closure can mutate it.
        let mut auto_approve_local = self.auto_approve_writes;
        let (submit, force_coding, toggle_changed) = ui.horizontal(|ui| {
            let can_act = !self.chat_pending && !self.chat_input.trim().is_empty();

            let send = ui.add_enabled(
                can_act,
                egui::Button::new("送出").min_size([60.0, 30.0].into()),
            );

            let code_btn = ui
                .add_enabled(
                    can_act,
                    egui::Button::new("⚙ 開發任務")
                        .min_size([100.0, 30.0].into())
                        .fill(Color32::from_rgb(25, 50, 80)),
                )
                .on_hover_text("強制以 Coding Agent 執行（跳過關鍵字偵測，直接進入 ReAct 迴圈）");

            // auto_approve_writes toggle — uses the cached value; saves to disk only on change.
            ui.separator();
            let toggle = ui
                .checkbox(&mut auto_approve_local, "自動允許寫入")
                .on_hover_text("關閉時，Coding Agent 寫入檔案前會彈出確認對話框（對應 persona.yaml 中的 auto_approve_writes）");

            // Plain Enter = newline; Shift+Enter = send.
            let enter_send = input.has_focus()
                && ui.input_mut(|i| {
                    if i.key_pressed(egui::Key::Enter) && i.modifiers.shift {
                        i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter);
                        true
                    } else {
                        false
                    }
                });

            (send.clicked() || enter_send, code_btn.clicked(), toggle.changed())
        })
        .inner;

        // Persist the toggle change if the user flipped it.
        if toggle_changed {
            self.auto_approve_writes = auto_approve_local;
            if let Ok(mut p) = crate::persona::Persona::load() {
                p.coding_agent.auto_approve_writes = auto_approve_local;
                let _ = p.save();
            }
        }

        // ── Force-coding path (⚙ 開發任務 button) ──────────────────────────
        if force_coding && !self.chat_input.trim().is_empty() {
            const MAX_TASK_CHARS: usize = 800;
            let raw = self.chat_input.trim().to_string();
            let task = if raw.chars().count() > MAX_TASK_CHARS {
                let t: String = raw.chars().take(MAX_TASK_CHARS).collect();
                format!("{t}…（已截斷）")
            } else {
                raw
            };
            self.chat_input.clear();
            self.chat_messages.push(ChatMessage {
                role: ChatRole::User,
                text: task.clone(),
            });
            self.chat_pending = true;
            self.coding_console = CodingConsoleState {
                status: "Coding Agent 啟動中…".to_string(),
                task: task.clone(),
                ..Default::default()
            };
            self.agent_console.route = "coding".to_string();
            self.agent_console.intent_family = "code_modification".to_string();
            self.agent_console.summary = "Coding Agent（強制觸發）".to_string();
            self.agent_console.steps = vec![
                "gather context".to_string(),
                "plan".to_string(),
                "ReAct loop".to_string(),
                "verify".to_string(),
            ];
            self.agent_console.recommended_skills = vec!["coding_agent".to_string()];
            self.agent_console.ai_details = crate::llm::shared_llm().task_log_summary();
            self.agent_console.status = "Coding Agent 執行中…".to_string();

            let tx = self.chat_tx.clone();
            let coding_tx = self.coding_tx.clone();
            let rt = self.rt.clone();
            rt.spawn(async move {
                let _ = tx.try_send(ChatUiUpdate {
                    reply: "⚙ Coding Agent 啟動中…".to_string(),
                    tools: vec![],
                    trace: vec![],
                    partial: true,
                    plan: Some(ChatPlanUpdate {
                        route: "coding".to_string(),
                        intent_family: "code_modification".to_string(),
                        summary: "Coding Agent（強制觸發，跳過 Planner / Router）".to_string(),
                        steps: vec![
                            "gather context".to_string(),
                            "plan".to_string(),
                            "ReAct loop".to_string(),
                            "verify".to_string(),
                        ],
                        recommended_skills: vec!["coding_agent".to_string()],
                    }),
                });
                let result = tokio::task::spawn(crate::agents::coding_agent::run_coding_via_adk(
                    task, false, None, None,
                ))
                .await;
                match result {
                    Ok(resp) => {
                        let _ = coding_tx.try_send(CodingUiUpdate {
                            response: Some(resp),
                            status_msg: "完成".to_string(),
                        });
                    }
                    Err(e) => {
                        // spawn 內部 panic — 解鎖 UI 並顯示錯誤
                        let _ = coding_tx.try_send(CodingUiUpdate {
                            response: Some(crate::agents::coding_agent::CodingAgentResponse {
                                outcome: format!("❌ Coding Agent 崩潰：{e}"),
                                result_status:
                                    crate::agents::coding_agent::CodingResultStatus::Error,
                                change_summary: "Coding Agent 執行時發生 panic，請查看 trace。"
                                    .to_string(),
                                files_modified: vec![],
                                iterations_used: 0,
                                trace: vec![],
                                diff: None,
                                verified: false,
                                verification_output: None,
                                dry_run: false,
                            }),
                            status_msg: "任務中止".to_string(),
                        });
                    }
                }
            });
        }

        // ── Normal submit path ───────────────────────────────────────────────
        if submit && !self.chat_input.trim().is_empty() {
            let user_text = self.chat_input.trim().to_string();

            // ── /skill command handling ───────────────────────────────────────
            if user_text.starts_with("/skill") {
                let arg = user_text["/skill".len()..].trim().to_string();
                self.chat_messages.push(ChatMessage {
                    role: ChatRole::User,
                    text: user_text.clone(),
                });
                self.chat_input.clear();
                let reply = if arg.is_empty() || arg == "list" {
                    let skills = crate::skills::list_skills();
                    let lines: Vec<String> = skills
                        .iter()
                        .map(|s| format!("• **{}** ({}): {}", s.id, s.category, s.description))
                        .collect();
                    format!("📦 可用技能清單：\n\n{}", lines.join("\n"))
                } else {
                    match crate::skills::execute_skill(&arg, &chrono::Utc::now().to_rfc3339()) {
                        Ok(result) => format!(
                            "✅ 技能 `{}` 已觸發\n事件：{}\n",
                            result.skill_id, result.emitted_event
                        ),
                        Err(e) => {
                            format!("❌ 技能執行失敗：{e}\n\n輸入 `/skill list` 查看可用技能。")
                        }
                    }
                };
                self.chat_messages.push(ChatMessage {
                    role: ChatRole::Assistant,
                    text: reply,
                });
            } else {
                // Check if this looks like a coding request that needs pre-flight confirmation.
                let is_coding_hint = crate::agents::router_agent::is_coding_request(&user_text);
                let needs_confirm = is_coding_hint && !self.auto_approve_writes;

                if needs_confirm && self.pending_coding_confirmation.is_none() {
                    // Show confirmation dialog — don't submit yet.
                    self.pending_coding_confirmation = Some(user_text);
                    // Push a pending indicator message so the user knows we saw the input.
                    self.chat_messages.push(ChatMessage {
                        role: ChatRole::User,
                        text: self
                            .pending_coding_confirmation
                            .as_deref()
                            .unwrap_or("")
                            .to_string(),
                    });
                    self.chat_input.clear();
                } else {
                    // Normal submit path (or confirmed coding request).
                    // If we came from the confirmation dialog, pending_coding_confirmation was
                    // already cleared by the "Allow" button handler — use user_text directly.
                    let confirmed_user_text = user_text.clone();

                    self.chat_messages.push(ChatMessage {
                        role: ChatRole::User,
                        text: confirmed_user_text.clone(),
                    });
                    self.chat_input.clear();
                    self.chat_pending = true;
                    // Only initialise the Coding Console when this actually looks like
                    // a coding request — for normal chat messages leave the previous
                    // console state intact so it doesn't flash up then disappear.
                    if is_coding_hint {
                        self.coding_console = CodingConsoleState {
                            status: "計畫中…".to_string(),
                            task: confirmed_user_text.clone(),
                            ..Default::default()
                        };
                    }

                    let is_meta_request =
                        crate::telegram::language::is_identity_question(&confirmed_user_text)
                            || crate::telegram::language::is_code_access_question(
                                &confirmed_user_text,
                            );

                    let history: Vec<String> = self
                        .chat_messages
                        .windows(2)
                        .filter_map(|pair| {
                            if pair[0].role == ChatRole::User && pair[1].role == ChatRole::Assistant
                            {
                                Some(format!(
                                    "User: {}\nAssistant: {}",
                                    pair[0].text, pair[1].text
                                ))
                            } else {
                                None
                            }
                        })
                        .rev()
                        .take(5)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    let context_block = if is_meta_request || history.is_empty() {
                        None
                    } else {
                        Some(history.join("\n---\n"))
                    };

                    let tx = self.chat_tx.clone();
                    let coding_tx = self.coding_tx.clone();
                    let rt = self.rt.clone();
                    self.agent_console.tools.clear();
                    self.agent_console.trace.clear();
                    self.agent_console.recommended_skills.clear();
                    self.agent_console.intent_family.clear();
                    if is_meta_request {
                        self.agent_console.route = "chat".to_string();
                        self.agent_console.intent_family = "capability".to_string();
                        self.agent_console.summary =
                            "直接回答身份 / 看碼能力問題，不啟動 research。".to_string();
                        self.agent_console.steps = vec![
                            "skip planner + router".to_string(),
                            "reply with local identity/capability summary".to_string(),
                        ];
                        self.agent_console.status = "直接回覆中…".to_string();
                    } else {
                        self.agent_console.route = "pending…".to_string();
                        self.agent_console.summary = String::new();
                        self.agent_console.steps.clear();
                        self.agent_console.status = "計畫中…".to_string();
                    }

                    let user_text_spawn = confirmed_user_text.clone();
                    rt.spawn(async move {
                        let plan_update = if is_meta_request {
                            ChatPlanUpdate {
                                route: "chat".to_string(),
                                intent_family: "capability".to_string(),
                                summary: "Direct capability/identity answer; no research workflow needed.".to_string(),
                                steps: vec![
                                    "skip planner + router".to_string(),
                                    "reply with local identity/capability summary".to_string(),
                                ],
                                recommended_skills: Vec::new(),
                            }
                        } else {
                            let plan = crate::agents::planner_agent::run_planner_via_adk(
                                crate::agents::planner_agent::PlannerRequest {
                                    user_text: user_text_spawn.clone(),
                                    context_block: context_block.clone(),
                                    peer_id: Some(0),
                                    fallback_reply: None,
                                    execution_result: None,
                                },
                                None,
                            )
                            .await
                            .ok();

                            plan.map(|p| ChatPlanUpdate {
                                route: match p.intent {
                                    crate::agents::planner_agent::PlanIntent::Research => "research".to_string(),
                                    crate::agents::planner_agent::PlanIntent::Answer => "chat".to_string(),
                                },
                                intent_family: serde_json::to_value(&p.intent_family)
                                    .ok()
                                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                                    .unwrap_or_else(|| "general_chat".to_string()),
                                summary: p.summary,
                                steps: p.steps,
                                recommended_skills: p.recommended_skills,
                            })
                            .unwrap_or_else(|| ChatPlanUpdate {
                                route: "chat".to_string(),
                                intent_family: "general_chat".to_string(),
                                summary: "Planner unavailable; using direct router fallback.".to_string(),
                                steps: vec!["route request".to_string(), "run chat response".to_string()],
                                recommended_skills: Vec::new(),
                            })
                        };

                        if is_meta_request {
                            let request = crate::agents::chat_agent::ChatRequest {
                                user_text: user_text_spawn.clone(),
                                execution_result: None,
                                context_block: None,
                                fallback_reply: None,
                                peer_id: Some(0),
                                planner_intent_family: None,
                                planner_skills: Vec::new(),
                                use_large_model: false,
                                agent_id: None,
                                disable_remote_ai: false,
                            };
                            run_chat_and_send(request, plan_update, user_text_spawn, tx).await;
                            return;
                        }

                        let routed = crate::agents::router_agent::run_router_via_adk(
                            crate::agents::router_agent::RouterRequest {
                                user_text: user_text_spawn.clone(),
                                context_block,
                                peer_id: Some(0),
                                fallback_reply: None,
                                execution_result: None,
                                agent_id: None,
                            },
                            None,
                        )
                        .await;

                        let output = match routed {
                            Ok(o) => o,
                            Err(err) => {
                                let _ = tx.send(ChatUiUpdate {
                                    reply: format!("路由錯誤：{err}"),
                                    tools: vec![],
                                    trace: vec![],
                                    partial: false,
                                    plan: Some(plan_update),
                                });
                                return;
                            }
                        };

                        let route = output
                            .get("route")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("chat");

                        if route == "coding" {
                            // ── Coding agent path ─────────────────────────────
                            let _ = tx.send(ChatUiUpdate {
                                reply: "⚙ Coding Agent 啟動中…".to_string(),
                                tools: vec![],
                                trace: vec![],
                                partial: true,
                                plan: Some(ChatPlanUpdate {
                                    route: "coding".to_string(),
                                    intent_family: "code_modification".to_string(),
                                    summary: "Running local AI Coding Agent (ReAct loop)…".to_string(),
                                    steps: vec![
                                        "gather context".to_string(),
                                        "plan".to_string(),
                                        "ReAct loop".to_string(),
                                        "verify".to_string(),
                                    ],
                                    recommended_skills: vec!["coding_agent".to_string()],
                                }),
                            });

                            let coding_request: crate::agents::coding_agent::CodingRequest =
                                output
                                    .get("coding_request")
                                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                                    .unwrap_or(crate::agents::coding_agent::CodingRequest {
                                        task: user_text_spawn.clone(),
                                        max_iterations: None,
                                        dry_run: false,
                                        context_block: None,
                                    });

                            let _ = coding_tx.try_send(CodingUiUpdate {
                                response: None,
                                status_msg: format!("🔄 執行中：{}", &coding_request.task),
                            });

                            let resp = crate::agents::coding_agent::run_coding_via_adk(
                                coding_request.task,
                                coding_request.dry_run,
                                None,
                                coding_request.context_block,
                            )
                            .await;

                            let _ = coding_tx.try_send(CodingUiUpdate {
                                response: Some(resp),
                                status_msg: "完成".to_string(),
                            });
                        } else {
                            // ── Chat / research agent path ────────────────────
                            let chat_request = output
                                .get("chat_request")
                                .cloned()
                                .unwrap_or_default();
                            let request = serde_json::from_value(chat_request)
                                .unwrap_or(crate::agents::chat_agent::ChatRequest {
                                    user_text: user_text_spawn.clone(),
                                    execution_result: None,
                                    context_block: None,
                                    fallback_reply: None,
                                    peer_id: Some(0),
                                    planner_intent_family: None,
                                    planner_skills: Vec::new(),
                                    use_large_model: false,
                                    agent_id: None,
                                    disable_remote_ai: false,
                                });
                            run_chat_and_send(request, plan_update, user_text_spawn, tx).await;
                        }
                    });
                }
            } // end else (not /skill command)
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        use crate::agent_config::AgentsFile;
        use crate::telegram_auth::TelegramStatus;

        // ── Lazy-load ─────────────────────────────────────────────────────────
        if self.settings_agents.is_none() {
            self.settings_agents = AgentsFile::load().ok().or_else(|| Some(AgentsFile::default()));
        }

        let mut do_save      = false;
        let mut do_reload    = false;
        let mut do_add_agent = false;
        let settings_msg = self.settings_msg.clone();

        let mut tg_code     = std::mem::take(&mut self.tg_code);
        let mut tg_password = std::mem::take(&mut self.tg_password);
        let tg_msg          = self.tg_msg.clone();
        let tg_auth         = self.tg_auth.clone();
        let mut tg_msg_update: Option<String> = None;

        let mut scratch        = std::mem::take(&mut self.settings_agent_scratch);
        let mut new_agent_id   = std::mem::take(&mut self.settings_new_agent_id);
        let mut new_agent_name = std::mem::take(&mut self.settings_new_agent_name);
        let mut selected_agent = self.settings_selected_agent;
        let mut selected_tab   = self.settings_active_tab;

        let agents_file = self.settings_agents.as_mut().unwrap();
        scratch.resize_with(agents_file.agents.len(), Default::default);

        let agent_auth_states: Vec<(String, crate::telegram_auth::TelegramAuthState)> =
            self.agent_auth_states.iter().map(|(id, s)| (id.clone(), s.clone())).collect();

        // ── Pre-collect sidebar rows (avoids borrow conflict with mut right panel) ──
        struct AgentRow { name: String, _id: String, enabled: bool, tg_status: Option<TelegramStatus> }
        let agent_rows: Vec<AgentRow> = agents_file.agents.iter().map(|a| {
            let tg_status = agent_auth_states.iter()
                .find(|(id, _)| id == &a.id)
                .map(|(_, s)| s.status());
            AgentRow { name: a.identity.name.clone(), _id: a.id.clone(), enabled: a.enabled, tg_status }
        }).collect();

        // Phone conflict map for channel tab
        let all_tg_phones: Vec<(String, String)> = agents_file.agents.iter()
            .filter_map(|a| {
                let phone = a.channel.as_ref()?.telegram.as_ref().map(|t| t.phone.clone())?;
                Some((a.id.clone(), phone))
            })
            .collect();

        // ── Toolbar ───────────────────────────────────────────────────────────
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.add(
                        egui::Button::new(RichText::new("💾  儲存").strong())
                            .fill(Color32::from_rgb(30, 90, 50)),
                    ).clicked() { do_save = true; }
                    if ui.button("↺  重新載入")
                        .on_hover_text("丟棄未儲存的變更，重新從磁碟讀取")
                        .clicked() { do_reload = true; }
                    if !settings_msg.is_empty() {
                        ui.separator();
                        let color = if settings_msg.starts_with('❌') {
                            Color32::from_rgb(220, 80, 80)
                        } else {
                            Color32::from_rgb(100, 220, 100)
                        };
                        ui.colored_label(color, &settings_msg);
                    }
                });
            });
        ui.separator();

        // ── Left sidebar ──────────────────────────────────────────────────────
        egui::SidePanel::left("settings_agents_sidebar")
            .resizable(true)
            .default_width(210.0)
            .min_width(150.0)
            .max_width(320.0)
            .show_inside(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut new_agent_id)
                        .hint_text("id").desired_width(65.0));
                    ui.add(egui::TextEdit::singleline(&mut new_agent_name)
                        .hint_text("名稱").desired_width(82.0));
                    let can_add = !new_agent_id.trim().is_empty() && !new_agent_name.trim().is_empty();
                    if ui.add_enabled(can_add, egui::Button::new("＋"))
                        .on_hover_text("新增 Agent").clicked() {
                        do_add_agent = true;
                    }
                });
                ui.separator();

                ScrollArea::vertical().id_salt("sidebar_scroll").show(ui, |ui| {
                    for (i, row) in agent_rows.iter().enumerate() {
                        let is_sel = selected_agent == Some(i);
                        let led_color = if row.enabled { Color32::from_rgb(80, 200, 100) } else { Color32::GRAY };
                        let clicked = egui::Frame::new()
                            .fill(if is_sel { Color32::from_rgb(35, 55, 80) } else { Color32::TRANSPARENT })
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(6, 3))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.colored_label(led_color, if row.enabled { "●" } else { "○" });
                                    ui.label(RichText::new(&row.name).strong());
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if let Some(ref st) = row.tg_status {
                                            match st {
                                                TelegramStatus::Connected =>
                                                    { ui.colored_label(Color32::from_rgb(80, 200, 100), "✈●"); }
                                                TelegramStatus::CodeRequired | TelegramStatus::PasswordRequired { .. } =>
                                                    { ui.colored_label(Color32::YELLOW, "✈⚠"); }
                                                TelegramStatus::Error { .. } =>
                                                    { ui.colored_label(Color32::from_rgb(220, 80, 80), "✈✗"); }
                                                TelegramStatus::Disconnected { .. } =>
                                                    { ui.colored_label(Color32::GRAY, "✈○"); }
                                            }
                                        }
                                    });
                                });
                            })
                            .response.interact(egui::Sense::click()).clicked();
                        if clicked { selected_agent = Some(i); }
                    }

                    ui.add_space(8.0);
                    ui.separator();

                    let is_sys = selected_agent.is_none();
                    if egui::Frame::new()
                        .fill(if is_sys { Color32::from_rgb(35, 55, 80) } else { Color32::TRANSPARENT })
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::symmetric(6, 3))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.colored_label(Color32::GRAY, "⚙  系統");
                        })
                        .response.interact(egui::Sense::click()).clicked()
                    {
                        selected_agent = None;
                    }
                });
            });

        // ── Right panel ───────────────────────────────────────────────────────
        ScrollArea::vertical().id_salt("settings_right").auto_shrink(false).show(ui, |ui| {
            ui.add_space(8.0);
            match selected_agent {
                None => {
                    show_system_panel(
                        ui, &tg_auth, &mut tg_code, &mut tg_password,
                        &tg_msg, &mut tg_msg_update,
                    );
                }
                Some(idx) if idx < agents_file.agents.len() => {
                    let agent = &mut agents_file.agents[idx];
                    let scratch_entry = &mut scratch[idx];
                    let auth_state = agent_auth_states.iter()
                        .find(|(id, _)| id == &agent.id)
                        .map(|(_, s)| s);
                    let other_tg_phones: Vec<(String, String)> = all_tg_phones.iter()
                        .filter(|(id, _)| id != &agent.id)
                        .cloned()
                        .collect();
                    show_agent_detail(
                        ui, agent, scratch_entry, auth_state,
                        &other_tg_phones, &mut selected_tab,
                    );
                }
                _ => {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(Color32::GRAY, "← 選擇左側 Agent 以編輯設定");
                    });
                }
            }
        });

        // ── Write-back ────────────────────────────────────────────────────────
        self.settings_agent_scratch  = scratch;
        self.settings_selected_agent = selected_agent;
        self.settings_active_tab     = selected_tab;
        self.tg_code                 = tg_code;
        self.tg_password             = tg_password;
        if let Some(msg) = tg_msg_update { self.tg_msg = msg; }

        if do_add_agent {
            let id   = new_agent_id.trim().to_string();
            let name = if new_agent_name.trim().is_empty() { id.clone() } else { new_agent_name.trim().to_string() };
            if let Some(f) = self.settings_agents.as_mut() {
                let new_idx = f.agents.len();
                f.agents.push(crate::agent_config::AgentConfig::new_default(&id, name));
                self.settings_selected_agent = Some(new_idx);
                self.settings_active_tab = 0;
            }
            new_agent_id.clear();
            new_agent_name.clear();
        }
        self.settings_new_agent_id   = new_agent_id;
        self.settings_new_agent_name = new_agent_name;

        if do_save {
            match self.settings_agents.as_ref().unwrap().save() {
                Ok(()) => self.settings_msg = "✅ 設定已儲存".to_string(),
                Err(e) => self.settings_msg = format!("❌ 儲存失敗：{e}"),
            }
            self.settings_msg_at = Some(std::time::Instant::now());
        }
        if do_reload {
            match AgentsFile::load() {
                Ok(fresh) => {
                    self.settings_agents = Some(fresh);
                    self.settings_agent_scratch.clear();
                    self.settings_msg = "已重新載入".to_string();
                }
                Err(e) => self.settings_msg = format!("❌ 載入失敗：{e}"),
            }
            self.settings_msg_at = Some(std::time::Instant::now());
        }
    }

    fn show_log(&mut self, ui: &mut egui::Ui) {
        // ── Header ────────────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.label(RichText::new("系統 Log").strong());
            ui.separator();
            ui.small(format!("{} 行", log_buffer::recent(300).len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("🗑 清除").clicked() {
                    log_buffer::clear();
                }
                if ui.small_button("📋 複製全部").clicked() {
                    ui.ctx().copy_text(log_buffer::snapshot_text(300));
                }
            });
        });
        ui.separator();

        ScrollArea::vertical()
            .id_salt("log_tab")
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                for line in log_buffer::recent(300) {
                    let color = if line.contains("[ERROR]")
                        || line.contains("error")
                        || line.contains("Error")
                        || line.contains("failed")
                        || line.contains("Failed")
                    {
                        Color32::from_rgb(220, 100, 100)
                    } else if line.contains("[WARN]") || line.contains("warn") {
                        Color32::from_rgb(220, 180, 80)
                    } else if line.contains("[telegram]") || line.contains("[tg]") {
                        Color32::from_rgb(100, 180, 255)
                    } else if line.contains("[researcher]") {
                        Color32::from_rgb(150, 220, 150)
                    } else if line.contains("[followup]") {
                        Color32::from_rgb(220, 180, 100)
                    } else if line.contains("[coding]") || line.contains("[adk]") {
                        Color32::from_rgb(180, 150, 255)
                    } else {
                        Color32::GRAY
                    };
                    ui.colored_label(color, egui::RichText::new(&line).monospace().small());
                }
            });
    }

    // ── Multi-Agent Dispatch / Management Panel ───────────────────────────────

    fn show_dispatch(&mut self, ui: &mut egui::Ui) {
        use crate::agent_config::AgentsFile;
        use crate::pending_reply::PendingStatus;
        use crate::telegram_auth::TelegramStatus;

        // Lazy-load agents config.
        if self.settings_agents.is_none() {
            self.settings_agents = AgentsFile::load().ok().or_else(|| Some(AgentsFile::default()));
        }
        let agents = match self.settings_agents.as_ref() {
            Some(f) => f.agents.clone(),
            None => Vec::new(),
        };

        // Default selection to the first agent.
        if self.dispatch_selected_agent.is_none() && !agents.is_empty() {
            self.dispatch_selected_agent = Some(0);
        }
        // Keep legacy dispatch_target_agent in sync.
        if let Some(idx) = self.dispatch_selected_agent {
            if let Some(agent) = agents.get(idx) {
                if self.dispatch_target_agent != agent.id {
                    self.dispatch_target_agent = agent.id.clone();
                }
            }
        }

        // ── Header ────────────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.label(RichText::new("🗂 多 Agent 調度台").heading().strong());
            ui.separator();
            ui.small(format!("{} 個 Agent 已配置", agents.len()));
            if !self.dispatch_msg.is_empty() {
                ui.separator();
                let color = if self.dispatch_msg.starts_with('❌') {
                    Color32::from_rgb(220, 80, 80)
                } else {
                    Color32::from_rgb(100, 220, 100)
                };
                ui.colored_label(color, &self.dispatch_msg);
            }
        });
        ui.separator();

        // ── Agent Fleet Grid ─────────────────────────────────────────────────
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(18, 24, 36))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.label(RichText::new("Agent 艦隊").strong().small());
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    for (agent_idx, agent) in agents.iter().enumerate() {
                        // Count tasks for this agent from the activity log.
                        let persona_name = &agent.identity.name;
                        let running = self.tasks.iter()
                            .filter(|t| &t.persona == persona_name
                                && matches!(t.status.as_deref(), Some("PENDING") | Some("RUNNING") | Some("FOLLOWING")))
                            .count();
                        let done = self.tasks.iter()
                            .filter(|t| &t.persona == persona_name && t.status.as_deref() == Some("DONE"))
                            .count();
                        let failed = self.tasks.iter()
                            .filter(|t| &t.persona == persona_name
                                && matches!(t.status.as_deref(), Some("FAILED") | Some("ERROR") | Some("FOLLOWUP_NEEDED")))
                            .count();
                        let tg_status = self.agent_auth_states.iter()
                            .find(|(id, _)| id == &agent.id)
                            .map(|(_, s)| s.status());
                        // Count pending confirmations for badge.
                        let pending_count = crate::pending_reply::load_pending(&agent.id)
                            .into_iter()
                            .filter(|r| r.status == PendingStatus::Pending)
                            .count();

                        let is_selected = self.dispatch_selected_agent == Some(agent_idx);
                        let card_bg = if is_selected {
                            Color32::from_rgb(30, 60, 100)
                        } else {
                            Color32::from_rgb(26, 32, 46)
                        };
                        let card_stroke = if is_selected {
                            egui::Stroke::new(1.5, Color32::from_rgb(80, 160, 255))
                        } else {
                            egui::Stroke::new(1.0, Color32::from_rgb(50, 60, 80))
                        };

                        let clicked = egui::Frame::new()
                            .fill(card_bg)
                            .stroke(card_stroke)
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.set_min_width(160.0);
                                ui.set_max_width(220.0);

                                // Agent name + status LED
                                ui.horizontal(|ui| {
                                    let led = if agent.enabled {
                                        Color32::from_rgb(80, 200, 100)
                                    } else {
                                        Color32::GRAY
                                    };
                                    ui.colored_label(led, "●");
                                    ui.label(RichText::new(&agent.identity.name).strong());

                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if let Some(ref st) = tg_status {
                                            match st {
                                                TelegramStatus::Connected =>
                                                    ui.colored_label(Color32::from_rgb(80, 200, 100), "✈●"),
                                                TelegramStatus::CodeRequired | TelegramStatus::PasswordRequired { .. } =>
                                                    ui.colored_label(Color32::YELLOW, "✈⚠"),
                                                TelegramStatus::Error { .. } =>
                                                    ui.colored_label(Color32::from_rgb(220, 80, 80), "✈✗"),
                                                TelegramStatus::Disconnected { .. } =>
                                                    ui.colored_label(Color32::GRAY, "✈○"),
                                            }
                                        } else {
                                            ui.colored_label(Color32::DARK_GRAY, "—")
                                        }
                                    });
                                });

                                ui.colored_label(Color32::DARK_GRAY, RichText::new(&agent.id).small().monospace());
                                ui.add_space(4.0);

                                // Task counters + pending badge
                                ui.horizontal(|ui| {
                                    if running > 0 {
                                        egui::Frame::new()
                                            .fill(Color32::from_rgb(70, 54, 18))
                                            .corner_radius(3.0)
                                            .inner_margin(egui::Margin::symmetric(5, 2))
                                            .show(ui, |ui| { ui.small(format!("⏳ {}", running)); });
                                    }
                                    if failed > 0 {
                                        egui::Frame::new()
                                            .fill(Color32::from_rgb(72, 24, 24))
                                            .corner_radius(3.0)
                                            .inner_margin(egui::Margin::symmetric(5, 2))
                                            .show(ui, |ui| { ui.small(format!("❌ {}", failed)); });
                                    }
                                    if pending_count > 0 {
                                        egui::Frame::new()
                                            .fill(Color32::from_rgb(70, 40, 10))
                                            .corner_radius(3.0)
                                            .inner_margin(egui::Margin::symmetric(5, 2))
                                            .show(ui, |ui| {
                                                ui.colored_label(Color32::from_rgb(255, 160, 60),
                                                    format!("📬 {}", pending_count));
                                            });
                                    }
                                    egui::Frame::new()
                                        .fill(Color32::from_rgb(22, 48, 24))
                                        .corner_radius(3.0)
                                        .inner_margin(egui::Margin::symmetric(5, 2))
                                        .show(ui, |ui| { ui.small(format!("✅ {}", done)); });
                                });

                                // Platform badge
                                let platform_label = match agent.platform() {
                                    crate::agent_config::AgentPlatform::Telegram => "✈ Telegram",
                                    crate::agent_config::AgentPlatform::Teams    => "💼 Teams",
                                    crate::agent_config::AgentPlatform::UiOnly   => "🖥 UI",
                                };
                                ui.colored_label(Color32::DARK_GRAY, RichText::new(platform_label).small());
                            })
                            .response
                            .interact(egui::Sense::click())
                            .clicked();

                        if clicked {
                            self.dispatch_selected_agent = Some(agent_idx);
                            self.dispatch_target_agent = agent.id.clone();
                            // Reset pending cache so it reloads for the new selection.
                            self.pending_replies_loaded_for.clear();
                        }
                    }

                    // Quick-add shortcut to Settings
                    let add_clicked = egui::Frame::new()
                        .fill(Color32::from_rgb(22, 28, 40))
                        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(50, 60, 80)))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_min_width(80.0);
                            ui.set_min_height(60.0);
                            ui.centered_and_justified(|ui| {
                                ui.colored_label(Color32::GRAY, "＋ 新增\nAgent");
                            });
                        })
                        .response
                        .interact(egui::Sense::click())
                        .clicked();
                    if add_clicked {
                        self.tab = Tab::Settings;
                        self.settings_selected_agent = Some(
                            self.settings_agents.as_ref().map(|f| f.agents.len()).unwrap_or(0).saturating_sub(1)
                        );
                    }
                });
            });

        ui.add_space(6.0);

        // ── Agent Detail Panel (tabs for selected agent) ──────────────────────
        if let Some(sel_idx) = self.dispatch_selected_agent {
            if let Some(agent) = agents.get(sel_idx) {
                let agent_id = agent.id.clone();
                let agent_name = agent.identity.name.clone();
                let kpi_defs = agent.kpi.metrics.clone();

                // Reload pending replies when agent selection changes.
                if self.pending_replies_loaded_for != agent_id {
                    self.pending_replies = crate::pending_reply::load_pending(&agent_id);
                    self.pending_replies_loaded_for = agent_id.clone();
                }
                let pending_count = self.pending_replies.iter()
                    .filter(|r| r.status == PendingStatus::Pending)
                    .count();

                egui::Frame::group(ui.style())
                    .fill(Color32::from_rgb(18, 25, 38))
                    .show(ui, |ui| {
                        // ── Tab bar ───────────────────────────────────────────
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("● {}", agent_name)).strong().small());
                            ui.separator();
                            let tabs: &[(&str, Option<usize>)] = &[
                                ("活動記錄", None),
                                ("記憶管理", None),
                                ("KPI 報表", None),
                                ("待確認", if pending_count > 0 { Some(pending_count) } else { None }),
                            ];
                            for (i, (label, badge)) in tabs.iter().enumerate() {
                                let is_active = self.dispatch_detail_tab == i;
                                let display = if let Some(n) = badge {
                                    format!("{} ({}) ⚠", label, n)
                                } else {
                                    label.to_string()
                                };
                                let btn = egui::Button::new(
                                    RichText::new(&display).small()
                                        .color(if is_active { Color32::WHITE } else { Color32::GRAY })
                                ).fill(if is_active { Color32::from_rgb(35, 65, 110) } else { Color32::TRANSPARENT });
                                if ui.add(btn).clicked() {
                                    self.dispatch_detail_tab = i;
                                }
                            }
                        });
                        ui.separator();

                        // ── Tab content ───────────────────────────────────────
                        ScrollArea::vertical()
                            .id_salt("dispatch_detail")
                            .max_height(280.0)
                            .auto_shrink(false)
                            .show(ui, |ui| {
                                match self.dispatch_detail_tab {
                                    // ── Tab 0: 活動記錄 ──────────────────────
                                    0 => {
                                        let filtered: Vec<_> = self.tasks.iter()
                                            .filter(|t| t.persona == agent_name)
                                            .take(40)
                                            .collect();
                                        if filtered.is_empty() {
                                            ui.colored_label(Color32::GRAY, "尚無活動記錄。");
                                        }
                                        for task in filtered {
                                            let status = task.status.as_deref().unwrap_or("—");
                                            let is_summary = task.event == "research_summary_ready";
                                            let (badge_text, badge_fg, badge_bg) =
                                                task_status_badge(status, task.reason.as_deref(), is_summary);
                                            let ts = task.timestamp.get(11..19).unwrap_or("—");
                                            let preview = task.message_preview.as_deref()
                                                .or(task.reason.as_deref())
                                                .unwrap_or(&task.event);
                                            ui.horizontal(|ui| {
                                                ui.colored_label(Color32::DARK_GRAY,
                                                    RichText::new(ts).small().monospace());
                                                egui::Frame::new()
                                                    .fill(badge_bg)
                                                    .stroke(egui::Stroke::new(1.0, badge_fg))
                                                    .inner_margin(egui::Margin::symmetric(4, 1))
                                                    .corner_radius(4.0)
                                                    .show(ui, |ui| {
                                                        ui.label(RichText::new(badge_text)
                                                            .small().strong().color(badge_fg));
                                                    });
                                                ui.label(RichText::new(
                                                    preview.chars().take(60).collect::<String>()).small());
                                            });
                                        }
                                    }

                                    // ── Tab 1: 記憶管理 ──────────────────────
                                    1 => {
                                        use crate::memory::StorageUsage;
                                        let s = &self.storage;
                                        egui::Grid::new("dispatch_mem_grid")
                                            .num_columns(3)
                                            .spacing([12.0, 4.0])
                                            .show(ui, |ui| {
                                                let rows: &[(&str, u64, bool)] = &[
                                                    ("對話 DB", s.memory_db_bytes, true),
                                                    ("代碼索引", s.call_graph_bytes, false),
                                                    ("調研記錄", s.research_log_bytes, true),
                                                    ("任務記錄", s.task_log_bytes, true),
                                                ];
                                                for (label, bytes, can_clear) in rows {
                                                    ui.label(*label);
                                                    ui.colored_label(
                                                        Color32::from_rgb(130, 200, 255),
                                                        StorageUsage::fmt_bytes(*bytes),
                                                    );
                                                    if *can_clear {
                                                        if ui.small_button("清除").clicked() {
                                                            // TODO: per-agent clear (currently global)
                                                            eprintln!("[ui] clear {label} — TODO: per-agent isolation");
                                                        }
                                                    } else {
                                                        ui.label("—");
                                                    }
                                                    ui.end_row();
                                                }
                                                ui.separator();
                                                ui.end_row();
                                                ui.label("總計");
                                                ui.colored_label(
                                                    Color32::WHITE,
                                                    StorageUsage::fmt_bytes(s.total_bytes),
                                                );
                                                ui.label("");
                                                ui.end_row();
                                            });
                                    }

                                    // ── Tab 2: KPI 報表 ──────────────────────
                                    2 => {
                                        ui.horizontal(|ui| {
                                            if ui.button("📊 從 API 更新 (TODO)").clicked() {
                                                self.dispatch_msg = "TODO: Agora API 未接線".to_string();
                                                self.dispatch_msg_at = Some(std::time::Instant::now());
                                            }
                                        });
                                        ui.add_space(4.0);

                                        // Load KPI values from disk if not cached.
                                        if !self.kpi_values.contains_key(&agent_id) {
                                            let vals = load_kpi_values(&agent_id);
                                            self.kpi_values.insert(agent_id.clone(), vals);
                                        }
                                        let kpi_vals = self.kpi_values.entry(agent_id.clone()).or_default();

                                        let mut save_kpi = false;
                                        if kpi_defs.is_empty() {
                                            ui.colored_label(Color32::GRAY,
                                                "尚未設定 KPI 指標。請在 ⚙ 設定 → Agent → KPI 新增。");
                                        } else {
                                            egui::Grid::new("dispatch_kpi_grid")
                                                .num_columns(3)
                                                .spacing([12.0, 4.0])
                                                .show(ui, |ui| {
                                                    ui.label(RichText::new("指標").strong().small());
                                                    ui.label(RichText::new("當前值").strong().small());
                                                    ui.label(RichText::new("操作").strong().small());
                                                    ui.end_row();
                                                    for def in &kpi_defs {
                                                        let val = kpi_vals.entry(def.key.clone()).or_default();
                                                        ui.label(&def.label);
                                                        let resp = ui.add(
                                                            egui::TextEdit::singleline(val)
                                                                .desired_width(80.0)
                                                                .hint_text("—"),
                                                        );
                                                        if resp.changed() { save_kpi = true; }
                                                        ui.label(RichText::new(&def.unit).small());
                                                        ui.end_row();
                                                    }
                                                });
                                        }
                                        if save_kpi {
                                            let vals_snapshot = kpi_vals.clone();
                                            save_kpi_values(&agent_id, &vals_snapshot);
                                        }
                                    }

                                    // ── Tab 3: 待確認 ────────────────────────
                                    3 => {
                                        let pending: Vec<_> = self.pending_replies.iter()
                                            .filter(|r| r.status == PendingStatus::Pending)
                                            .cloned()
                                            .collect();
                                        if pending.is_empty() {
                                            ui.colored_label(Color32::GRAY, "沒有待確認的草稿。");
                                        }
                                        // Collect actions to avoid borrow conflict.
                                        let mut approve_id: Option<String> = None;
                                        let mut reject_id: Option<String> = None;
                                        let mut delete_id: Option<String> = None;

                                        for pr in &pending {
                                            egui::Frame::new()
                                                .fill(Color32::from_rgb(24, 30, 44))
                                                .stroke(egui::Stroke::new(1.0, Color32::from_rgb(60, 80, 110)))
                                                .corner_radius(6.0)
                                                .inner_margin(egui::Margin::symmetric(10, 6))
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.label(RichText::new(format!(
                                                            "💬 {}  {}  {}",
                                                            pr.peer_name, pr.platform,
                                                            pr.created_at.get(11..16).unwrap_or("—")
                                                        )).small().strong());
                                                    });
                                                    ui.colored_label(Color32::GRAY,
                                                        RichText::new(format!(
                                                            "原始: \"{}\"",
                                                            pr.original_message.chars().take(80).collect::<String>()
                                                        )).small());
                                                    ui.separator();
                                                    ui.label(RichText::new("草稿：").small());
                                                    let draft = self.pending_draft_edits
                                                        .entry(pr.id.clone())
                                                        .or_insert_with(|| pr.draft_reply.clone());
                                                    let available_w = ui.available_width();
                                                    ui.add(
                                                        egui::TextEdit::multiline(draft)
                                                            .desired_width(available_w)
                                                            .desired_rows(3),
                                                    );
                                                    ui.add_space(4.0);
                                                    ui.horizontal(|ui| {
                                                        if ui.add(egui::Button::new(
                                                            RichText::new("✅ 確認發送").small())
                                                            .fill(Color32::from_rgb(25, 70, 35)))
                                                            .clicked()
                                                        {
                                                            approve_id = Some(pr.id.clone());
                                                        }
                                                        if ui.add(egui::Button::new(
                                                            RichText::new("❌ 拒絕").small())
                                                            .fill(Color32::from_rgb(72, 24, 24)))
                                                            .clicked()
                                                        {
                                                            reject_id = Some(pr.id.clone());
                                                        }
                                                        if ui.add(egui::Button::new(
                                                            RichText::new("🗑 刪除").small())
                                                            .fill(Color32::TRANSPARENT))
                                                            .clicked()
                                                        {
                                                            delete_id = Some(pr.id.clone());
                                                        }
                                                    });
                                                });
                                            ui.add_space(4.0);
                                        }

                                        // Apply actions.
                                        if let Some(id) = approve_id {
                                            // Apply any edited draft text before approving.
                                            if let Some(edited) = self.pending_draft_edits.get(&id) {
                                                let mut replies = crate::pending_reply::load_pending(&agent_id);
                                                if let Some(r) = replies.iter_mut().find(|r| r.id == id) {
                                                    r.draft_reply = edited.clone();
                                                    r.status = PendingStatus::Approved;
                                                }
                                                let _ = crate::pending_reply::save_pending(&agent_id, &replies);
                                                self.pending_draft_edits.remove(&id);
                                            } else {
                                                crate::pending_reply::update_status(&agent_id, &id, PendingStatus::Approved);
                                            }
                                            self.dispatch_msg = "✅ 已批准（TODO: 實際發送需 Telegram session）".to_string();
                                            self.dispatch_msg_at = Some(std::time::Instant::now());
                                            self.pending_replies = crate::pending_reply::load_pending(&agent_id);
                                        }
                                        if let Some(id) = reject_id {
                                            crate::pending_reply::update_status(&agent_id, &id, PendingStatus::Rejected);
                                            self.pending_draft_edits.remove(&id);
                                            self.pending_replies = crate::pending_reply::load_pending(&agent_id);
                                        }
                                        if let Some(id) = delete_id {
                                            crate::pending_reply::delete_pending(&agent_id, &id);
                                            self.pending_draft_edits.remove(&id);
                                            self.pending_replies = crate::pending_reply::load_pending(&agent_id);
                                        }
                                    }
                                    _ => {}
                                }
                            });
                    });
            }
        }

        ui.add_space(4.0);
        ui.separator();

        // ── Operations area (Dispatch form) ──────────────────────────────────
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(18, 24, 36))
            .show(ui, |ui| {
                ui.label(RichText::new("操作").strong().small());
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    // Target agent selector
                    ui.label("目標：");
                    egui::ComboBox::from_id_salt("dispatch_agent_combo")
                        .selected_text(&self.dispatch_target_agent)
                        .width(140.0)
                        .show_ui(ui, |ui| {
                            for agent in &agents {
                                let led = if agent.enabled { "● " } else { "○ " };
                                ui.selectable_value(
                                    &mut self.dispatch_target_agent,
                                    agent.id.clone(),
                                    format!("{}{}", led, agent.identity.name),
                                );
                            }
                        });
                    ui.separator();
                    ui.label("對象：");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.dispatch_manual_peer)
                            .hint_text("peer 名稱 / ID（選填）")
                            .desired_width(160.0),
                    );
                });

                let available_w = ui.available_width();
                ui.add(
                    egui::TextEdit::multiline(&mut self.dispatch_task_input)
                        .hint_text("輸入訊息、任務或調研主題…")
                        .desired_width(available_w)
                        .desired_rows(3),
                );

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let can_send = !self.dispatch_task_input.trim().is_empty();
                    if ui.add_enabled(can_send,
                        egui::Button::new(RichText::new("▶ 手動發送").strong())
                            .fill(Color32::from_rgb(30, 70, 130)))
                        .clicked()
                    {
                        let task_text = self.dispatch_task_input.trim().to_string();
                        let agent_id = self.dispatch_target_agent.clone();
                        let prefix = format!("[{}] ", agent_id);
                        self.chat_input = format!("{}{}", prefix, task_text);
                        self.tab = Tab::Chat;
                        self.dispatch_task_input.clear();
                    }
                    if ui.add_enabled(can_send,
                        egui::Button::new(RichText::new("🔬 先調研再回").small())
                            .fill(Color32::TRANSPARENT))
                        .clicked()
                    {
                        let topic = self.dispatch_task_input.trim().to_string();
                        let rt = self.rt.clone();
                        rt.spawn(async move {
                            crate::agents::research_agent::run_research_via_adk(topic, None).await;
                        });
                        self.tab = Tab::Research;
                        self.dispatch_msg = "✅ 已啟動調研".to_string();
                        self.dispatch_msg_at = Some(std::time::Instant::now());
                        self.dispatch_task_input.clear();
                    }
                    if ui.add_enabled(can_send,
                        egui::Button::new(RichText::new("⚙ 編碼任務").small())
                            .fill(Color32::TRANSPARENT))
                        .clicked()
                    {
                        let task_text = self.dispatch_task_input.trim().to_string();
                        let agent_id = self.dispatch_target_agent.clone();
                        self.chat_input = format!("[{}] ⚙ {}", agent_id, task_text);
                        self.tab = Tab::Chat;
                        self.dispatch_task_input.clear();
                    }
                });
            });
    }
}
// ── Agent detail (right panel) ────────────────────────────────────────────────

fn show_agent_detail(
    ui: &mut egui::Ui,
    agent: &mut crate::agent_config::AgentConfig,
    scratch: &mut AgentUiScratch,
    auth: Option<&crate::telegram_auth::TelegramAuthState>,
    other_tg_phones: &[(String, String)],
    active_tab: &mut usize,
) {
    use crate::telegram_auth::TelegramStatus;

    // ── Agent header ──────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        let enabled_color = if agent.enabled { Color32::from_rgb(80, 200, 100) } else { Color32::GRAY };
        ui.colored_label(enabled_color, if agent.enabled { "●" } else { "○" });
        ui.label(RichText::new(&agent.identity.name).heading().strong());
        ui.colored_label(Color32::GRAY, format!("  {}", agent.id));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (label, color) = if agent.enabled {
                ("ON", Color32::from_rgb(80, 200, 100))
            } else {
                ("OFF", Color32::from_rgb(120, 120, 120))
            };
            if ui.add(egui::Button::new(RichText::new(label).color(color)).frame(true)).clicked() {
                agent.enabled = !agent.enabled;
            }
            if let Some(a) = auth {
                match a.status() {
                    TelegramStatus::Connected =>
                        { ui.colored_label(Color32::from_rgb(80, 200, 100), "✈●"); }
                    TelegramStatus::CodeRequired | TelegramStatus::PasswordRequired { .. } =>
                        { ui.colored_label(Color32::YELLOW, "✈⚠"); }
                    TelegramStatus::Error { .. } =>
                        { ui.colored_label(Color32::from_rgb(220, 80, 80), "✈✗"); }
                    TelegramStatus::Disconnected { .. } =>
                        { ui.colored_label(Color32::GRAY, "✈○"); }
                }
            }
        });
    });
    ui.separator();

    // ── Tab bar ───────────────────────────────────────────────────────────
    let tabs = ["身分", "風格", "目標", "通訊", "能力", "行為"];
    ui.horizontal(|ui| {
        for (i, tab_name) in tabs.iter().enumerate() {
            let is_active = *active_tab == i;
            let text = if is_active {
                RichText::new(*tab_name).strong()
            } else {
                RichText::new(*tab_name).color(Color32::GRAY)
            };
            let btn = egui::Button::new(text)
                .fill(if is_active { Color32::from_rgb(40, 80, 130) } else { Color32::TRANSPARENT });
            if ui.add(btn).clicked() {
                *active_tab = i;
            }
        }
    });
    ui.separator();

    // ── Tab content ───────────────────────────────────────────────────────
    ui.add_space(4.0);
    match *active_tab {
        0 => show_tab_identity(ui, agent),
        1 => show_tab_style(ui, agent),
        2 => show_tab_goals(ui, agent, scratch),
        3 => show_tab_channel(ui, agent, auth, other_tg_phones),
        4 => show_tab_actions(ui, agent, scratch),
        5 => show_tab_behavior(ui, agent),
        _ => {}
    }
}

fn show_tab_identity(ui: &mut egui::Ui, agent: &mut crate::agent_config::AgentConfig) {
    use crate::persona::ProfessionalTone;
    egui::Grid::new("tab_identity")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("名稱");
            ui.add(egui::TextEdit::singleline(&mut agent.identity.name).desired_width(240.0));
            ui.end_row();

            ui.label("語氣");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut agent.identity.professional_tone, ProfessionalTone::Brief, "簡潔");
                ui.selectable_value(&mut agent.identity.professional_tone, ProfessionalTone::Detailed, "詳細");
                ui.selectable_value(&mut agent.identity.professional_tone, ProfessionalTone::Casual, "輕鬆");
            });
            ui.end_row();

        });
}

fn show_tab_style(ui: &mut egui::Ui, agent: &mut crate::agent_config::AgentConfig) {
    egui::Grid::new("tab_style")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("語音風格").on_hover_text("AI 回覆的整體語氣描述，嵌入系統提示");
            ui.add(egui::TextEdit::singleline(&mut agent.response_style.voice)
                .desired_width(f32::INFINITY));
            ui.end_row();

            ui.label("確認前綴").on_hover_text("收到訊息時的開頭確認語（{ack_prefix} 佔位符）");
            ui.add(egui::TextEdit::singleline(&mut agent.response_style.ack_prefix)
                .desired_width(f32::INFINITY));
            ui.end_row();

            ui.label("合規提示").on_hover_text("附加在自動回覆模板的合規聲明");
            ui.add(egui::TextEdit::singleline(&mut agent.response_style.compliance_line)
                .desired_width(f32::INFINITY));
            ui.end_row();
        });
}

fn show_tab_goals(
    ui: &mut egui::Ui,
    agent: &mut crate::agent_config::AgentConfig,
    scratch: &mut AgentUiScratch,
) {
    let mut remove_idx: Option<usize> = None;
    for (j, obj) in agent.objectives.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.colored_label(Color32::from_rgb(120, 180, 255), format!("{}", j + 1));
            ui.label(obj);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button(RichText::new("✕").color(Color32::from_rgb(180, 70, 70))).clicked() {
                    remove_idx = Some(j);
                }
            });
        });
    }
    if let Some(j) = remove_idx { agent.objectives.remove(j); }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut scratch.new_objective)
            .hint_text("新增目標…").desired_width(300.0));
        if ui.button("＋").clicked() && !scratch.new_objective.trim().is_empty() {
            agent.objectives.push(scratch.new_objective.trim().to_string());
            scratch.new_objective.clear();
        }
    });
}

fn show_tab_channel(
    ui: &mut egui::Ui,
    agent: &mut crate::agent_config::AgentConfig,
    auth: Option<&crate::telegram_auth::TelegramAuthState>,
    other_tg_phones: &[(String, String)],
) {
    use crate::telegram_auth::TelegramStatus;
    use crate::agent_config::{ChannelConfig, TelegramChannelConfig};

    // ── Live status (buttons only, no duplicated text from header badge) ──
    if let Some(a) = auth {
        ui.horizontal(|ui| {
            match &a.status() {
                TelegramStatus::Connected => {
                    ui.colored_label(Color32::from_rgb(80, 200, 100), "● 已連線");
                }
                TelegramStatus::Disconnected { reason } => {
                    ui.colored_label(Color32::GRAY, format!("○ 未連線（{reason}）"));
                    if ui.small_button("🔌 連線").clicked() { a.trigger_reconnect(); }
                }
                TelegramStatus::Error { message } => {
                    ui.colored_label(Color32::from_rgb(220, 80, 80), format!("✗ 錯誤：{message}"));
                    if ui.small_button("🔌 重試").clicked() { a.trigger_reconnect(); }
                }
                TelegramStatus::CodeRequired => {
                    ui.colored_label(Color32::YELLOW, "⚠ 等待驗證碼（至系統面板輸入）");
                }
                TelegramStatus::PasswordRequired { hint } => {
                    ui.colored_label(Color32::YELLOW, format!("⚠ 等待 2FA（提示：{hint}）"));
                }
            }
        });
        ui.separator();
    }

    // ── Telegram toggle ───────────────────────────────────────────────────
    let has_tg = agent.channel.as_ref().and_then(|c| c.telegram.as_ref()).is_some();
    let mut tg_enabled = has_tg;
    if ui.checkbox(&mut tg_enabled, "Telegram").changed() {
        if tg_enabled {
            let id_slug = agent.id.clone();
            let cfg = agent.channel.get_or_insert_with(ChannelConfig::default);
            cfg.telegram = Some(TelegramChannelConfig {
                session_file: format!("data/sessions/{id_slug}.session"),
                ..TelegramChannelConfig::default()
            });
        } else if let Some(c) = agent.channel.as_mut() {
            c.telegram = None;
            if c.telegram.is_none() { agent.channel = None; }
        }
    }

    if let Some(tg) = agent.channel.as_mut().and_then(|c| c.telegram.as_mut()) {
        // ── Conflict check ────────────────────────────────────────────────
        let phone_trimmed = tg.phone.trim().to_string();
        let conflict_agent = if !phone_trimmed.is_empty() && !phone_trimmed.starts_with("${") {
            other_tg_phones.iter().find(|(_, p)| p.trim() == phone_trimmed).map(|(id, _)| id.clone())
        } else {
            None
        };
        if let Some(ref other_id) = conflict_agent {
            ui.add_space(4.0);
            egui::Frame::new()
                .fill(Color32::from_rgb(80, 40, 10))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.colored_label(
                        Color32::from_rgb(255, 180, 60),
                        format!("⚠ 電話號碼已被 Agent「{other_id}」使用"),
                    );
                });
        }

        ui.add_space(6.0);
        egui::Grid::new("tg_cfg")
            .num_columns(2)
            .spacing([12.0, 5.0])
            .show(ui, |ui| {
                ui.label("API ID");
                ui.add(egui::TextEdit::singleline(&mut tg.api_id)
                    .desired_width(200.0).hint_text("${TG_API_ID}"));
                ui.end_row();

                ui.label("API Hash");
                ui.add(egui::TextEdit::singleline(&mut tg.api_hash)
                    .desired_width(200.0).hint_text("${TG_API_HASH}"));
                ui.end_row();

                ui.label("電話號碼");
                let phone_edit = egui::TextEdit::singleline(&mut tg.phone)
                    .desired_width(200.0).hint_text("+886...");
                if conflict_agent.is_some() {
                    ui.add(phone_edit.text_color(Color32::from_rgb(255, 180, 60)));
                } else {
                    ui.add(phone_edit);
                }
                ui.end_row();

                ui.label("Session 路徑");
                ui.add(egui::TextEdit::singleline(&mut tg.session_file)
                    .desired_width(f32::INFINITY));
                ui.end_row();
            });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut tg.reply_private, "回覆私訊");
            ui.checkbox(&mut tg.reply_groups, "回覆群組");
            ui.checkbox(&mut tg.auto_reply, "自動回覆");
        });
        ui.add_space(6.0);
        ui.colored_label(
            Color32::GRAY,
            "ℹ 每個 Telegram 帳號（電話號碼）只能綁定一個 Agent",
        );
    } else {
        ui.add_space(8.0);
        ui.colored_label(Color32::GRAY, "（無 Channel — UI / 測試模式）");
    }
}

fn show_tab_actions(
    ui: &mut egui::Ui,
    agent: &mut crate::agent_config::AgentConfig,
    _scratch: &mut AgentUiScratch,
) {
    // ── Capability toggles ────────────────────────────────────────────────
    ui.add_space(4.0);
    ui.checkbox(&mut agent.actions.research_agent.enabled, "🔬 Research Agent");
    ui.add_space(4.0);
    ui.checkbox(&mut agent.actions.coding_agent.enabled, "⚙ Coding Agent");
    ui.add_space(4.0);
    ui.checkbox(&mut agent.disable_remote_ai, "🖥 關閉遠端 AI（強制使用本地 LLM）");

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(4.0);
    egui::Frame::new()
        .fill(Color32::from_rgb(28, 32, 44))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.colored_label(
                Color32::GRAY,
                "📝 詳細 coding 設定（project_root、max_iterations、auto_approve 等）\n請修改 config/persona.yaml → coding_agent。",
            );
        });
}

fn show_tab_behavior(ui: &mut egui::Ui, agent: &mut crate::agent_config::AgentConfig) {
    use crate::agent_config::{BreakPeriod, WorkSchedule};
    let hb = &mut agent.human_behavior;

    ui.checkbox(&mut hb.enabled, "啟用人類行為模擬");
    if !hb.enabled {
        ui.add_space(4.0);
        ui.colored_label(Color32::GRAY, "（停用時 AI 會即時回覆，不套用任何限制）");
        return;
    }

    ui.add_space(8.0);
    ui.separator();
    ui.label(RichText::new("回覆延遲").strong().small());
    egui::Grid::new("hb_delay_grid").num_columns(4).spacing([8.0, 4.0]).show(ui, |ui| {
        ui.label("最短");
        let mut min_s = hb.min_reply_delay_secs as i64;
        if ui.add(egui::DragValue::new(&mut min_s).range(0..=3600).suffix("秒")).changed() {
            hb.min_reply_delay_secs = min_s.max(0) as u64;
            if hb.min_reply_delay_secs > hb.max_reply_delay_secs {
                hb.max_reply_delay_secs = hb.min_reply_delay_secs;
            }
        }
        ui.label("最長");
        let mut max_s = hb.max_reply_delay_secs as i64;
        if ui.add(egui::DragValue::new(&mut max_s).range(0..=3600).suffix("秒")).changed() {
            hb.max_reply_delay_secs = max_s.max(0) as u64;
            if hb.max_reply_delay_secs < hb.min_reply_delay_secs {
                hb.min_reply_delay_secs = hb.max_reply_delay_secs;
            }
        }
        ui.end_row();
    });

    ui.add_space(8.0);
    ui.label(RichText::new("訊息頻率限制").strong().small());
    egui::Grid::new("hb_freq_grid").num_columns(4).spacing([8.0, 4.0]).show(ui, |ui| {
        ui.label("每小時最多");
        ui.add(egui::DragValue::new(&mut hb.max_messages_per_hour).range(0..=1000).suffix("則"));
        ui.label("每天最多");
        ui.add(egui::DragValue::new(&mut hb.max_messages_per_day).range(0..=10000).suffix("則"));
        ui.end_row();
    });

    ui.add_space(8.0);
    ui.separator();

    // Work schedule toggle
    let has_schedule = hb.work_schedule.is_some();
    let mut enable_sched = has_schedule;
    ui.checkbox(&mut enable_sched, "啟用工作時間限制");
    if enable_sched && !has_schedule {
        hb.work_schedule = Some(WorkSchedule::default());
    } else if !enable_sched && has_schedule {
        hb.work_schedule = None;
    }

    if let Some(ref mut sched) = hb.work_schedule {
        ui.add_space(4.0);
        egui::Grid::new("hb_sched_grid").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.label("時區偏移 UTC");
            ui.add(egui::DragValue::new(&mut sched.utc_offset_hours)
                .range(-12..=14).prefix("+"));
            ui.end_row();
            ui.label("上班時間");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut sched.work_start).desired_width(60.0));
                ui.label("—");
                ui.add(egui::TextEdit::singleline(&mut sched.work_end).desired_width(60.0));
            });
            ui.end_row();
        });

        // Work days checkboxes
        ui.horizontal(|ui| {
            ui.label("工作日：");
            let day_names = ["一", "二", "三", "四", "五", "六", "日"];
            for (i, name) in day_names.iter().enumerate() {
                let day_num = (i + 1) as u8;
                let mut checked = sched.work_days.contains(&day_num);
                if ui.checkbox(&mut checked, *name).changed() {
                    if checked {
                        if !sched.work_days.contains(&day_num) {
                            sched.work_days.push(day_num);
                            sched.work_days.sort();
                        }
                    } else {
                        sched.work_days.retain(|&d| d != day_num);
                    }
                }
            }
        });

        // Break periods
        ui.add_space(8.0);
        ui.label(RichText::new("休息時段").strong().small());
        let mut remove_idx: Option<usize> = None;
        for (i, brk) in sched.breaks.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut brk.name).desired_width(60.0).hint_text("名稱"));
                ui.add(egui::TextEdit::singleline(&mut brk.start).desired_width(55.0).hint_text("HH:MM"));
                ui.label("—");
                ui.add(egui::TextEdit::singleline(&mut brk.end).desired_width(55.0).hint_text("HH:MM"));
                if ui.small_button("✕").clicked() {
                    remove_idx = Some(i);
                }
            });
        }
        if let Some(i) = remove_idx {
            sched.breaks.remove(i);
        }
        if ui.small_button("＋ 新增休息時段").clicked() {
            sched.breaks.push(BreakPeriod {
                name: "休息".to_string(),
                start: "12:00".to_string(),
                end:   "13:00".to_string(),
            });
        }
    }

    // require_confirmation
    ui.add_space(8.0);
    ui.separator();
    ui.label(RichText::new("人工確認").strong().small());
    let tg_require = agent.channel.as_mut()
        .and_then(|c| c.telegram.as_mut())
        .map(|t| &mut t.require_confirmation);
    if let Some(require_conf) = tg_require {
        ui.checkbox(require_conf, "需要人工確認回覆（AI 草稿不直接發送，等待確認）");
    } else {
        ui.colored_label(Color32::GRAY, "（此 Agent 未配置 Telegram 頻道）");
    }
}

fn show_system_panel(
    ui: &mut egui::Ui,
    tg_auth: &crate::telegram_auth::TelegramAuthState,
    tg_code: &mut String,
    tg_password: &mut String,
    tg_msg: &str,
    tg_msg_update: &mut Option<String>,
) {
    use crate::telegram_auth::TelegramStatus;

    ui.label(RichText::new("⚙  系統").heading().strong());
    ui.separator();
    ui.add_space(6.0);

    // ── LLM backend ───────────────────────────────────────────────────────
    ui.label(RichText::new("LLM 後端").strong());
    ui.add_space(2.0);
    let main_llm  = crate::llm::shared_llm();
    let large_llm = crate::llm::shared_large_llm();
    egui::Grid::new("sys_ai_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("主模型");
        let (c, icon) = if main_llm.is_remote() {
            (Color32::from_rgb(255, 160, 60), "☁")
        } else {
            (Color32::from_rgb(100, 220, 100), "🖥")
        };
        ui.colored_label(c, format!("{icon}  {} / {}", main_llm.backend_name(), main_llm.model));
        ui.end_row();
        if large_llm.model != main_llm.model {
            ui.label("大模型");
            let (c, icon) = if large_llm.is_remote() {
                (Color32::from_rgb(255, 160, 60), "☁")
            } else {
                (Color32::from_rgb(100, 220, 100), "🖥")
            };
            ui.colored_label(c, format!("{icon}  {} / {}", large_llm.backend_name(), large_llm.model));
            ui.end_row();
        }
    });
    ui.add_space(2.0);
    ui.colored_label(Color32::GRAY, "LLM 後端與 env vars 需修改 .env 後重啟生效");

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(6.0);

    // ── Telegram auth ─────────────────────────────────────────────────────
    let tg_status = tg_auth.status();
    let (status_color, status_text) = match &tg_status {
        TelegramStatus::Connected               => (Color32::from_rgb(100, 220, 100), "● 已連線"),
        TelegramStatus::Disconnected { .. }     => (Color32::GRAY,                    "○ 未連線"),
        TelegramStatus::CodeRequired            => (Color32::YELLOW,                  "⚠ 需要驗證碼"),
        TelegramStatus::PasswordRequired { .. } => (Color32::YELLOW,                  "⚠ 需要 2FA"),
        TelegramStatus::Error { .. }            => (Color32::from_rgb(220, 80, 80),   "✗ 錯誤"),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("✈  Telegram").strong());
        ui.colored_label(status_color, status_text);
        if let TelegramStatus::Disconnected { reason } = &tg_status {
            ui.small(reason.as_str());
        }
        if let TelegramStatus::Error { message } = &tg_status {
            ui.small(message.as_str());
        }
    });
    ui.add_space(4.0);

    match &tg_status {
        TelegramStatus::CodeRequired => {
            let submitted = ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(tg_code)
                    .desired_width(160.0).hint_text("驗證碼"));
                let b = ui.button("提交");
                (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || b.clicked()
            }).inner;
            if submitted && !tg_code.trim().is_empty() {
                tg_auth.submit_code(tg_code.trim().to_string());
                tg_code.clear();
                *tg_msg_update = Some("驗證碼已提交".to_string());
            }
        }
        TelegramStatus::PasswordRequired { hint } => {
            ui.label(format!("2FA（提示：{hint}）："));
            let submitted = ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(tg_password)
                    .password(true).desired_width(160.0));
                let b = ui.button("提交");
                (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || b.clicked()
            }).inner;
            if submitted && !tg_password.trim().is_empty() {
                tg_auth.submit_password(tg_password.clone());
                tg_password.clear();
                *tg_msg_update = Some("密碼已提交".to_string());
            }
        }
        TelegramStatus::Disconnected { .. } | TelegramStatus::Error { .. } => {
            let has_api_id   = std::env::var("TG_API_ID").is_ok();
            let has_api_hash = std::env::var("TG_API_HASH").is_ok();
            let has_phone    = std::env::var("TG_PHONE").map(|v| !v.trim().is_empty()).unwrap_or(false);
            if !has_api_id || !has_api_hash {
                ui.colored_label(Color32::from_rgb(220, 160, 60), "⚠ 缺少 TG_API_ID / TG_API_HASH");
            } else if !has_phone {
                ui.colored_label(Color32::from_rgb(220, 160, 60), "⚠ 缺少 TG_PHONE");
            }
            let can_connect = has_api_id && has_api_hash && has_phone;
            if ui.add_enabled(
                can_connect,
                egui::Button::new(RichText::new("🔌  立即連線").strong())
                    .fill(Color32::from_rgb(25, 60, 100)),
            ).clicked() {
                tg_auth.trigger_reconnect();
                *tg_msg_update = Some("已觸發連線，等待驗證碼…".to_string());
            }
        }
        TelegramStatus::Connected => {}
    }

    if !tg_msg.is_empty() {
        ui.colored_label(Color32::from_rgb(100, 220, 100), tg_msg);
    }
}

// ── KPI persistence helpers ───────────────────────────────────────────────────

fn kpi_path(agent_id: &str) -> std::path::PathBuf {
    std::path::PathBuf::from("data").join("kpi").join(format!("{agent_id}.json"))
}

fn load_kpi_values(agent_id: &str) -> std::collections::HashMap<String, String> {
    let path = kpi_path(agent_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_kpi_values(agent_id: &str, vals: &std::collections::HashMap<String, String>) {
    let path = kpi_path(agent_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(vals) {
        let _ = std::fs::write(&path, json);
    }
}

async fn run_chat_and_send(
    request: crate::agents::chat_agent::ChatRequest,
    plan_update: ChatPlanUpdate,
    user_text: String,
    tx: std::sync::mpsc::SyncSender<ChatUiUpdate>,
) {
    let tx_partial = tx.clone();
    let accumulated = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let acc_clone = std::sync::Arc::clone(&accumulated);
    let plan_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let plan_sent_clone = std::sync::Arc::clone(&plan_sent);
    let plan_update_clone = plan_update.clone();

    let response = crate::agents::chat_agent::stream_chat_response(request, move |token| {
        if let Ok(mut acc) = acc_clone.lock() {
            acc.push_str(&token);
            let preview = format!("{} ▍", acc.trim_end());
            let plan = if !plan_sent_clone.swap(true, std::sync::atomic::Ordering::Relaxed) {
                Some(plan_update_clone.clone())
            } else {
                None
            };
            let _ = tx_partial.try_send(ChatUiUpdate {
                reply: preview,
                tools: vec![],
                trace: vec![],
                partial: true,
                plan,
            });
        }
    })
    .await;

    let _ = crate::memory::append_context(&user_text, &response.reply, Some(0), None);
    let final_plan = if !plan_sent.load(std::sync::atomic::Ordering::Relaxed) {
        Some(plan_update)
    } else {
        None
    };
    let _ = tx.send(ChatUiUpdate {
        reply: response.reply,
        tools: response.tools_used,
        trace: response.trace,
        partial: false,
        plan: final_plan,
    });
}
