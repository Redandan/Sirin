//! Native egui/eframe UI for Sirin.
//!
//! Runs on the main thread. Background Tokio tasks (Telegram listener,
//! follow-up worker) communicate via the same shared-state structs they
//! always have — no IPC layer needed.

use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, RichText, ScrollArea,
};
use tokio::runtime::Handle;
use tokio::sync::broadcast;

use crate::events::AgentEvent;
use crate::log_buffer;
use crate::memory::ensure_codebase_index;
use crate::persona::{TaskEntry, TaskTracker};
use crate::researcher::{self, ResearchTask};
use crate::telegram_auth::TelegramAuthState;

// ── Top-level view selector ───────────────────────────────────────────────────

#[derive(PartialEq, Clone)]
enum View {
    /// Agent workspace — None means "show first agent".
    Agent(Option<usize>),
    Settings,
    Log,
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
    view: View,

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

    // Chat / operations
    chat_messages: Vec<ChatMessage>,
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

    // ── UI state ──────────────────────────────────────────────────────────────
    /// Filter applied in the workspace 活動 sub-tab.
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

    // ── Operations / Dispatch state ───────────────────────────────────────────
    /// Currently selected agent id for operations ("" = first agent).
    dispatch_target_agent: String,
    /// Text input buffer for manual dispatch / chat.
    dispatch_task_input: String,
    /// Feedback message shown after dispatching.
    dispatch_msg: String,
    dispatch_msg_at: Option<std::time::Instant>,

    // ── Agent workspace ───────────────────────────────────────────────────────
    /// Active sub-tab in the workspace: 0=活動, 1=調研, 2=KPI, 3=待確認, 4=操作.
    workspace_tab: usize,
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
    /// Pending reply counts cached per agent (refreshed every 5 s).  Avoids disk reads every frame.
    pending_count_cache: std::collections::HashMap<String, usize>,
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
            view: View::Agent(Some(0)),
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
            dispatch_task_input: String::new(),
            dispatch_msg: String::new(),
            dispatch_msg_at: None,
            workspace_tab: 4,
            dispatch_manual_peer: String::new(),
            pending_replies: Vec::new(),
            pending_replies_loaded_for: String::new(),
            pending_draft_edits: std::collections::HashMap::new(),
            kpi_values: std::collections::HashMap::new(),
            pending_count_cache: std::collections::HashMap::new(),
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
        // Refresh pending reply counts for all configured agents (avoids per-frame disk reads).
        if let Some(f) = &self.settings_agents {
            for agent in &f.agents {
                let count = crate::pending_reply::load_pending(&agent.id)
                    .into_iter()
                    .filter(|r| r.status == crate::pending_reply::PendingStatus::Pending)
                    .count();
                self.pending_count_cache.insert(agent.id.clone(), count);
            }
        }
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
                    // Switch to workspace → 調研 sub-tab and kick off the research run.
                    self.view = View::Agent(self.view_agent_idx());
                    self.workspace_tab = 1;
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
                    let sel_id = self.view_agent_idx()
                        .and_then(|i| self.settings_agents.as_ref()?.agents.get(i))
                        .map(|a| a.id.clone());
                    if sel_id.as_deref() == Some(agent_id.as_str()) {
                        self.pending_replies = crate::pending_reply::load_pending(&agent_id);
                        self.pending_replies_loaded_for = agent_id.clone();
                    }
                    // Switch focus to workspace → 待確認 sub-tab.
                    self.view = View::Agent(self.view_agent_idx());
                    self.workspace_tab = 3;
                }
                Ok(_) => {} // other events (ResearchCompleted, FollowupTriggered, ChatAgentReplied)
                Err(broadcast::error::TryRecvError::Lagged(_)) => {} // skip lagged events
                Err(_) => break, // Empty or Closed
            }
        }

        // ── Left sidebar ──────────────────────────────────────────────────────
        {
            // Lazy-load agents for the sidebar list.
            if self.settings_agents.is_none() {
                use crate::agent_config::AgentsFile;
                self.settings_agents = AgentsFile::load().ok().or_else(|| Some(AgentsFile::default()));
            }
            let agents: Vec<_> = self.settings_agents.as_ref()
                .map(|f| f.agents.iter().map(|a| (a.id.clone(), a.identity.name.clone(), a.enabled)).collect())
                .unwrap_or_default();
            let pending_count_cache = self.pending_count_cache.clone();
            let cur_view = self.view.clone();

            egui::SidePanel::left("main_sidebar")
                .resizable(false)
                .exact_width(172.0)
                .show(ctx, |ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Sirin").heading().strong());
                    ui.add_space(2.0);
                    ui.separator();

                    ScrollArea::vertical()
                        .id_salt("sidebar_agents")
                        .max_height(ui.available_height() - 82.0)
                        .show(ui, |ui| {
                            for (i, (agent_id, agent_name, enabled)) in agents.iter().enumerate() {
                                let is_sel = cur_view == View::Agent(Some(i));
                                let led = if *enabled {
                                    Color32::from_rgb(80, 200, 100)
                                } else {
                                    Color32::GRAY
                                };
                                let pending_n = *pending_count_cache.get(agent_id).unwrap_or(&0);
                                let clicked = egui::Frame::new()
                                    .fill(if is_sel { Color32::from_rgb(35, 55, 80) } else { Color32::TRANSPARENT })
                                    .corner_radius(4.0)
                                    .inner_margin(egui::Margin::symmetric(6, 3))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.colored_label(led, "●");
                                            ui.label(RichText::new(agent_name).strong());
                                            if pending_n > 0 {
                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                    ui.colored_label(
                                                        Color32::from_rgb(255, 160, 60),
                                                        format!("📬{}", pending_n),
                                                    );
                                                });
                                            }
                                        });
                                    })
                                    .response
                                    .interact(egui::Sense::click())
                                    .clicked();
                                if clicked {
                                    self.view = View::Agent(Some(i));
                                    self.pending_replies_loaded_for.clear();
                                    // Sync dispatch_target_agent
                                    self.dispatch_target_agent = agent_id.clone();
                                }
                            }
                        });

                    // Bottom nav items
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        let has_err = log_buffer::recent(50)
                            .iter()
                            .any(|l| l.contains("[ERROR]") || l.contains("error") || l.contains("failed"));
                        let log_sel = cur_view == View::Log;
                        if egui::Frame::new()
                            .fill(if log_sel { Color32::from_rgb(35, 55, 80) } else { Color32::TRANSPARENT })
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(6, 3))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                let lbl = if has_err { "📋 Log ●" } else { "📋 Log" };
                                ui.colored_label(
                                    if log_sel { Color32::WHITE } else { Color32::GRAY },
                                    lbl,
                                );
                            })
                            .response
                            .interact(egui::Sense::click())
                            .clicked()
                        {
                            self.view = View::Log;
                        }

                        let sett_sel = cur_view == View::Settings;
                        if egui::Frame::new()
                            .fill(if sett_sel { Color32::from_rgb(35, 55, 80) } else { Color32::TRANSPARENT })
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(6, 3))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.colored_label(
                                    if sett_sel { Color32::WHITE } else { Color32::GRAY },
                                    "⚙ 設定",
                                );
                            })
                            .response
                            .interact(egui::Sense::click())
                            .clicked()
                        {
                            self.view = View::Settings;
                        }

                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.small_button("⟳").on_hover_text("立即刷新").clicked() {
                                self.refresh();
                            }
                            let secs = self.last_refresh.elapsed().as_secs();
                            ui.small(format!("{secs}s 前"));
                        });
                    });
                });
        }

        // ── Central panel ─────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| match self.view.clone() {
            View::Agent(idx) => self.show_agent_workspace(ui, idx),
            View::Settings   => self.show_settings(ui),
            View::Log        => self.show_log(ui),
        });
    }
}

// ── Panel rendering ───────────────────────────────────────────────────────────

impl SirinApp {
    // ── View helpers ──────────────────────────────────────────────────────────

    /// Returns the agent index currently shown (or 0 as fallback).
    fn view_agent_idx(&self) -> Option<usize> {
        if let View::Agent(i) = self.view { i } else { Some(0) }
    }

    // ── Agent Workspace ───────────────────────────────────────────────────────

    fn show_agent_workspace(&mut self, ui: &mut egui::Ui, sel: Option<usize>) {
        use crate::agent_config::AgentsFile;
        use crate::pending_reply::PendingStatus;

        if self.settings_agents.is_none() {
            self.settings_agents = AgentsFile::load().ok().or_else(|| Some(AgentsFile::default()));
        }
        let agents = match self.settings_agents.as_ref() {
            Some(f) => f.agents.clone(),
            None => Vec::new(),
        };
        let sel = match sel { Some(i) if i < agents.len() => i, _ => {
            if agents.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.colored_label(Color32::GRAY, "← 點選左側 Agent，或在 ⚙ 設定 中新增");
                });
                return;
            }
            0
        }};
        let agent = agents[sel].clone();
        let agent_id = agent.id.clone();
        let agent_name = agent.identity.name.clone();
        let kpi_defs = agent.kpi.metrics.clone();

        // Keep workspace selection in sync.
        if self.pending_replies_loaded_for != agent_id {
            self.pending_replies = crate::pending_reply::load_pending(&agent_id);
            self.pending_replies_loaded_for = agent_id.clone();
        }
        if self.dispatch_target_agent != agent_id {
            self.dispatch_target_agent = agent_id.clone();
        }
        let pending_count = self.pending_replies.iter()
            .filter(|r| r.status == PendingStatus::Pending)
            .count();

        // ── Agent header ──────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            let led = if agent.enabled { Color32::from_rgb(80, 200, 100) } else { Color32::GRAY };
            ui.colored_label(led, "●");
            ui.label(RichText::new(&agent_name).heading().strong());
            ui.colored_label(Color32::GRAY, format!("  {}", agent_id));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let tg_status = self.agent_auth_states.iter()
                    .find(|(id, _)| id == &agent_id)
                    .map(|(_, s)| s.status());
                if let Some(ref st) = tg_status { tg_status_badge(ui, st); }
                let platform_label = match agent.platform() {
                    crate::agent_config::AgentPlatform::Telegram => "✈ Telegram",
                    crate::agent_config::AgentPlatform::Teams    => "💼 Teams",
                    crate::agent_config::AgentPlatform::UiOnly   => "🖥 UI",
                };
                ui.colored_label(Color32::DARK_GRAY, RichText::new(platform_label).small());
            });
        });
        if !self.dispatch_msg.is_empty() {
            let color = if self.dispatch_msg.starts_with('❌') {
                Color32::from_rgb(220, 80, 80)
            } else {
                Color32::from_rgb(100, 220, 100)
            };
            ui.colored_label(color, &self.dispatch_msg);
        }
        ui.separator();

        // ── Sub-tab bar ───────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            let tabs: &[(&str, Option<usize>)] = &[
                ("活動", None),
                ("調研", None),
                ("KPI",  None),
                ("待確認", if pending_count > 0 { Some(pending_count) } else { None }),
                ("操作",  None),
            ];
            for (i, (label, badge)) in tabs.iter().enumerate() {
                let is_active = self.workspace_tab == i;
                let display = if let Some(n) = badge {
                    format!("{} ({}) ⚠", label, n)
                } else {
                    label.to_string()
                };
                let btn = egui::Button::new(
                    RichText::new(&display)
                        .color(if is_active { Color32::WHITE } else { Color32::GRAY }),
                )
                .fill(if is_active { Color32::from_rgb(35, 65, 110) } else { Color32::TRANSPARENT });
                if ui.add(btn).clicked() {
                    self.workspace_tab = i;
                }
            }
        });
        ui.separator();

        // ── Sub-tab content ───────────────────────────────────────────────────
        match self.workspace_tab {
            // ── 活動 ──────────────────────────────────────────────────────────
            0 => self.show_tasks_for_agent(ui, &agent_name.clone()),

            // ── 調研 ──────────────────────────────────────────────────────────
            1 => self.show_research_workspace(ui),

            // ── KPI ───────────────────────────────────────────────────────────
            2 => {
                ui.horizontal(|ui| {
                    if ui.button("📊 從 API 更新 (TODO)").clicked() {
                        self.dispatch_msg = "TODO: Agora API 未接線".to_string();
                        self.dispatch_msg_at = Some(std::time::Instant::now());
                    }
                });
                ui.add_space(4.0);
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
                    egui::Grid::new("ws_kpi_grid")
                        .num_columns(3)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            ui.label(RichText::new("指標").strong().small());
                            ui.label(RichText::new("當前值").strong().small());
                            ui.label(RichText::new("單位").strong().small());
                            ui.end_row();
                            for def in &kpi_defs {
                                let val = kpi_vals.entry(def.key.clone()).or_default();
                                ui.label(&def.label);
                                if ui.add(
                                    egui::TextEdit::singleline(val)
                                        .desired_width(80.0)
                                        .hint_text("—"),
                                ).changed() { save_kpi = true; }
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

            // ── 待確認 ────────────────────────────────────────────────────────
            3 => {
                let pending: Vec<_> = self.pending_replies.iter()
                    .filter(|r| r.status == PendingStatus::Pending)
                    .cloned()
                    .collect();
                if pending.is_empty() {
                    ui.colored_label(Color32::GRAY, "沒有待確認的草稿。");
                }
                let mut approve_id: Option<String> = None;
                let mut reject_id: Option<String> = None;
                let mut delete_id: Option<String> = None;

                ScrollArea::vertical().id_salt("ws_pending").auto_shrink(false).show(ui, |ui| {
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
                                ui.colored_label(Color32::GRAY, RichText::new(format!(
                                    "原始: \"{}\"",
                                    pr.original_message.chars().take(80).collect::<String>()
                                )).small());
                                ui.separator();
                                ui.label(RichText::new("草稿：").small());
                                let draft = self.pending_draft_edits
                                    .entry(pr.id.clone())
                                    .or_insert_with(|| pr.draft_reply.clone());
                                let aw = ui.available_width();
                                ui.add(egui::TextEdit::multiline(draft).desired_width(aw).desired_rows(3));
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    if ui.add(egui::Button::new(RichText::new("✅ 確認發送").small())
                                        .fill(Color32::from_rgb(25, 70, 35))).clicked()
                                    { approve_id = Some(pr.id.clone()); }
                                    if ui.add(egui::Button::new(RichText::new("❌ 拒絕").small())
                                        .fill(Color32::from_rgb(72, 24, 24))).clicked()
                                    { reject_id = Some(pr.id.clone()); }
                                    if ui.add(egui::Button::new(RichText::new("🗑 刪除").small())
                                        .fill(Color32::TRANSPARENT)).clicked()
                                    { delete_id = Some(pr.id.clone()); }
                                });
                            });
                        ui.add_space(4.0);
                    }
                });

                if let Some(id) = approve_id {
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

            // ── 操作 (chat + dispatch) ────────────────────────────────────────
            4 | _ => {
                self.show_operations_tab(ui, &agents);
            }
        }
    }

    /// 操作 sub-tab: dispatch form + chat input + chat messages.
    fn show_operations_tab(&mut self, ui: &mut egui::Ui, agents: &[crate::agent_config::AgentConfig]) {
        // ── Dispatch form ─────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.label("目標：");
            egui::ComboBox::from_id_salt("ws_agent_combo")
                .selected_text(&self.dispatch_target_agent)
                .width(140.0)
                .show_ui(ui, |ui| {
                    for agent in agents {
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
                    .desired_width(150.0),
            );
        });

        let available_w = ui.available_width();
        let input = ui.add(
            egui::TextEdit::multiline(&mut self.dispatch_task_input)
                .hint_text("輸入訊息、任務或調研主題…（Enter 送出，Shift+Enter 換行）")
                .desired_width(available_w)
                .desired_rows(2),
        );

        let mut auto_approve_local = self.auto_approve_writes;
        let (submit, do_research, force_coding, toggle_changed) = ui.horizontal(|ui| {
            let can_act = !self.dispatch_task_input.trim().is_empty() && !self.chat_pending;
            let send = ui.add_enabled(
                can_act,
                egui::Button::new(RichText::new("▶ 送出").strong())
                    .min_size([70.0, 28.0].into())
                    .fill(Color32::from_rgb(30, 70, 130)),
            );
            let research_btn = ui.add_enabled(
                can_act,
                egui::Button::new(RichText::new("🔬 調研").small())
                    .fill(Color32::TRANSPARENT),
            );
            let code_btn = ui.add_enabled(
                can_act,
                egui::Button::new(RichText::new("⚙ 編碼").small())
                    .fill(Color32::TRANSPARENT),
            ).on_hover_text("強制以 Coding Agent 執行");
            ui.separator();
            let toggle = ui
                .checkbox(&mut auto_approve_local, "自動允許寫入")
                .on_hover_text("關閉時，Coding Agent 寫入前會確認（persona.yaml auto_approve_writes）");
            let enter_send = input.has_focus()
                && ui.input_mut(|i| {
                    if i.key_pressed(egui::Key::Enter) && i.modifiers.shift {
                        i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter);
                        true
                    } else { false }
                });
            (send.clicked() || enter_send, research_btn.clicked(), code_btn.clicked(), toggle.changed())
        }).inner;

        if toggle_changed {
            self.auto_approve_writes = auto_approve_local;
            if let Ok(mut p) = crate::persona::Persona::load() {
                p.coding_agent.auto_approve_writes = auto_approve_local;
                let _ = p.save();
            }
        }

        // ── 🔬 Research path ──────────────────────────────────────────────────
        if do_research && !self.dispatch_task_input.trim().is_empty() {
            let topic = self.dispatch_task_input.trim().to_string();
            let rt = self.rt.clone();
            rt.spawn(async move {
                crate::agents::research_agent::run_research_via_adk(topic, None).await;
            });
            self.workspace_tab = 1; // switch to 調研 sub-tab
            self.dispatch_msg = "✅ 已啟動調研".to_string();
            self.dispatch_msg_at = Some(std::time::Instant::now());
            self.dispatch_task_input.clear();
        }

        // ── ⚙ Force-coding path ───────────────────────────────────────────────
        if force_coding && !self.dispatch_task_input.trim().is_empty() {
            let task = self.dispatch_task_input.trim().to_string();
            self.dispatch_task_input.clear();
            self.chat_messages.push(ChatMessage { role: ChatRole::User, text: task.clone() });
            self.chat_pending = true;
            self.coding_console = CodingConsoleState {
                status: "Coding Agent 啟動中…".to_string(),
                task: task.clone(),
                ..Default::default()
            };
            self.agent_console.route = "coding".to_string();
            self.agent_console.status = "Coding Agent 執行中…".to_string();
            self.agent_console.ai_details = crate::llm::shared_llm().task_log_summary();
            let coding_tx = self.coding_tx.clone();
            let tx = self.chat_tx.clone();
            let rt = self.rt.clone();
            rt.spawn(async move {
                let _ = tx.try_send(ChatUiUpdate {
                    reply: "⚙ Coding Agent 啟動中…".to_string(),
                    tools: vec![], trace: vec![], partial: true,
                    plan: Some(ChatPlanUpdate {
                        route: "coding".to_string(),
                        intent_family: "code_modification".to_string(),
                        summary: "Coding Agent（強制觸發）".to_string(),
                        steps: vec!["gather context".to_string(), "plan".to_string(), "ReAct loop".to_string(), "verify".to_string()],
                        recommended_skills: vec!["coding_agent".to_string()],
                    }),
                });
                let result = tokio::task::spawn(
                    crate::agents::coding_agent::run_coding_via_adk(task, false, None, None)
                ).await;
                let resp = match result {
                    Ok(r) => r,
                    Err(e) => crate::agents::coding_agent::CodingAgentResponse {
                        outcome: format!("❌ Coding Agent 崩潰：{e}"),
                        result_status: crate::agents::coding_agent::CodingResultStatus::Error,
                        change_summary: String::new(),
                        files_modified: vec![], iterations_used: 0, trace: vec![],
                        diff: None, verified: false, verification_output: None, dry_run: false,
                    },
                };
                let _ = coding_tx.try_send(CodingUiUpdate { response: Some(resp), status_msg: "完成".to_string() });
            });
        }

        // ── ▶ Normal submit path ──────────────────────────────────────────────
        if submit && !self.dispatch_task_input.trim().is_empty() {
            let user_text = self.dispatch_task_input.trim().to_string();
            let is_coding_hint = crate::agents::router_agent::is_coding_request(&user_text);
            let needs_confirm = is_coding_hint && !self.auto_approve_writes;

            if needs_confirm && self.pending_coding_confirmation.is_none() {
                self.pending_coding_confirmation = Some(user_text.clone());
                self.chat_messages.push(ChatMessage { role: ChatRole::User, text: user_text });
                self.dispatch_task_input.clear();
            } else {
                let confirmed_text = user_text.clone();
                self.chat_messages.push(ChatMessage { role: ChatRole::User, text: confirmed_text.clone() });
                self.dispatch_task_input.clear();
                self.chat_pending = true;
                if is_coding_hint {
                    self.coding_console = CodingConsoleState {
                        status: "計畫中…".to_string(),
                        task: confirmed_text.clone(),
                        ..Default::default()
                    };
                }
                let is_meta = crate::telegram::language::is_identity_question(&confirmed_text)
                    || crate::telegram::language::is_code_access_question(&confirmed_text);
                let history: Vec<String> = self.chat_messages.windows(2)
                    .filter_map(|p| {
                        if p[0].role == ChatRole::User && p[1].role == ChatRole::Assistant {
                            Some(format!("User: {}\nAssistant: {}", p[0].text, p[1].text))
                        } else { None }
                    })
                    .rev().take(5).collect::<Vec<_>>().into_iter().rev().collect();
                let context_block = if is_meta || history.is_empty() { None } else { Some(history.join("\n---\n")) };
                let tx = self.chat_tx.clone();
                let coding_tx = self.coding_tx.clone();
                let rt = self.rt.clone();
                self.agent_console.tools.clear();
                self.agent_console.trace.clear();
                self.agent_console.recommended_skills.clear();
                self.agent_console.intent_family.clear();
                self.agent_console.route = "pending…".to_string();
                self.agent_console.status = "計畫中…".to_string();
                self.agent_console.ai_details = crate::llm::shared_llm().task_log_summary();
                let user_text_spawn = confirmed_text.clone();
                rt.spawn(async move {
                    let plan_update = if is_meta {
                        ChatPlanUpdate {
                            route: "chat".to_string(), intent_family: "capability".to_string(),
                            summary: "Direct capability/identity answer.".to_string(),
                            steps: vec!["skip planner + router".to_string()],
                            recommended_skills: Vec::new(),
                        }
                    } else {
                        let plan = crate::agents::planner_agent::run_planner_via_adk(
                            crate::agents::planner_agent::PlannerRequest {
                                user_text: user_text_spawn.clone(),
                                context_block: context_block.clone(),
                                peer_id: Some(0), fallback_reply: None, execution_result: None,
                            }, None,
                        ).await.ok();
                        plan.map(|p| ChatPlanUpdate {
                            route: match p.intent {
                                crate::agents::planner_agent::PlanIntent::Research => "research".to_string(),
                                crate::agents::planner_agent::PlanIntent::Answer => "chat".to_string(),
                            },
                            intent_family: serde_json::to_value(&p.intent_family).ok()
                                .and_then(|v| v.as_str().map(|s| s.to_string()))
                                .unwrap_or_else(|| "general_chat".to_string()),
                            summary: p.summary, steps: p.steps,
                            recommended_skills: p.recommended_skills,
                        }).unwrap_or_else(|| ChatPlanUpdate {
                            route: "chat".to_string(), intent_family: "general_chat".to_string(),
                            summary: "Planner unavailable.".to_string(),
                            steps: vec!["route request".to_string()],
                            recommended_skills: Vec::new(),
                        })
                    };

                    if is_meta {
                        let request = crate::agents::chat_agent::ChatRequest {
                            user_text: user_text_spawn.clone(), execution_result: None,
                            context_block: None, fallback_reply: None, peer_id: Some(0),
                            planner_intent_family: None, planner_skills: Vec::new(),
                            use_large_model: false, agent_id: None, disable_remote_ai: false,
                        };
                        run_chat_and_send(request, plan_update, user_text_spawn, tx).await;
                        return;
                    }

                    let routed = crate::agents::router_agent::run_router_via_adk(
                        crate::agents::router_agent::RouterRequest {
                            user_text: user_text_spawn.clone(), context_block,
                            peer_id: Some(0), fallback_reply: None,
                            execution_result: None, agent_id: None,
                        }, None,
                    ).await;

                    let output = match routed {
                        Ok(o) => o,
                        Err(err) => {
                            let _ = tx.send(ChatUiUpdate {
                                reply: format!("路由錯誤：{err}"), tools: vec![], trace: vec![],
                                partial: false, plan: Some(plan_update),
                            });
                            return;
                        }
                    };
                    let route = output.get("route").and_then(serde_json::Value::as_str).unwrap_or("chat");
                    if route == "coding" {
                        let _ = tx.send(ChatUiUpdate {
                            reply: "⚙ Coding Agent 啟動中…".to_string(), tools: vec![], trace: vec![],
                            partial: true,
                            plan: Some(ChatPlanUpdate {
                                route: "coding".to_string(), intent_family: "code_modification".to_string(),
                                summary: "Running Coding Agent…".to_string(),
                                steps: vec!["gather context".to_string(), "plan".to_string(), "ReAct loop".to_string(), "verify".to_string()],
                                recommended_skills: vec!["coding_agent".to_string()],
                            }),
                        });
                        let coding_request: crate::agents::coding_agent::CodingRequest =
                            output.get("coding_request")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                                .unwrap_or(crate::agents::coding_agent::CodingRequest {
                                    task: user_text_spawn.clone(),
                                    max_iterations: None, dry_run: false, context_block: None,
                                });
                        let _ = coding_tx.try_send(CodingUiUpdate {
                            response: None,
                            status_msg: format!("🔄 執行中：{}", &coding_request.task),
                        });
                        let resp = crate::agents::coding_agent::run_coding_via_adk(
                            coding_request.task, coding_request.dry_run, None, coding_request.context_block,
                        ).await;
                        let _ = coding_tx.try_send(CodingUiUpdate {
                            response: Some(resp), status_msg: "完成".to_string(),
                        });
                    } else {
                        let chat_request = output.get("chat_request").cloned().unwrap_or_default();
                        let request = serde_json::from_value(chat_request)
                            .unwrap_or(crate::agents::chat_agent::ChatRequest {
                                user_text: user_text_spawn.clone(), execution_result: None,
                                context_block: None, fallback_reply: None, peer_id: Some(0),
                                planner_intent_family: None, planner_skills: Vec::new(),
                                use_large_model: false, agent_id: None, disable_remote_ai: false,
                            });
                        run_chat_and_send(request, plan_update, user_text_spawn, tx).await;
                    }
                });
            }
        }

        ui.add_space(6.0);
        ui.separator();

        // ── Coding pre-flight confirmation dialog ─────────────────────────────
        if let Some(ref pending_task) = self.pending_coding_confirmation.clone() {
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(40, 30, 10))
                .show(ui, |ui| {
                    ui.label(RichText::new("⚠ Coding Agent 需要寫入檔案權限").color(Color32::YELLOW).strong());
                    ui.small(pending_task.chars().take(100).collect::<String>());
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("✅ 允許寫入").color(Color32::from_rgb(100, 220, 100))).clicked() {
                            let task_clone = pending_task.clone();
                            self.pending_coding_confirmation = None;
                            let coding_tx = self.coding_tx.clone();
                            let tx = self.chat_tx.clone();
                            let rt = self.rt.clone();
                            self.chat_pending = true;
                            self.coding_console = CodingConsoleState {
                                status: "執行中（允許寫入）…".to_string(),
                                task: task_clone.clone(), ..Default::default()
                            };
                            self.agent_console.ai_details = crate::llm::shared_llm().task_log_summary();
                            rt.spawn(async move {
                                let _ = tx.try_send(ChatUiUpdate {
                                    reply: "⚙ Coding Agent 啟動中（允許寫入）…".to_string(),
                                    tools: vec![], trace: vec![], partial: true,
                                    plan: Some(ChatPlanUpdate {
                                        route: "coding".to_string(),
                                        intent_family: "code_modification".to_string(),
                                        summary: "Running Coding Agent with write permission…".to_string(),
                                        steps: vec!["gather context".to_string(), "plan".to_string(), "ReAct loop".to_string(), "verify".to_string()],
                                        recommended_skills: vec!["coding_agent".to_string()],
                                    }),
                                });
                                let resp = crate::agents::coding_agent::run_coding_via_adk(task_clone, false, None, None).await;
                                let _ = coding_tx.try_send(CodingUiUpdate { response: Some(resp), status_msg: "完成".to_string() });
                            });
                        }
                        if ui.button(RichText::new("👁 Dry-run").color(Color32::from_rgb(100, 180, 255))).clicked() {
                            let task_clone = pending_task.clone();
                            self.pending_coding_confirmation = None;
                            let coding_tx = self.coding_tx.clone();
                            let rt = self.rt.clone();
                            self.chat_pending = true;
                            self.coding_console = CodingConsoleState {
                                status: "Dry-run 執行中…".to_string(),
                                task: task_clone.clone(), ..Default::default()
                            };
                            rt.spawn(async move {
                                let resp = crate::agents::coding_agent::run_coding_via_adk(task_clone, true, None, None).await;
                                let _ = coding_tx.try_send(CodingUiUpdate { response: Some(resp), status_msg: "Dry-run 完成".to_string() });
                            });
                        }
                        if ui.button(RichText::new("❌ 取消").color(Color32::from_rgb(220, 80, 80))).clicked() {
                            self.pending_coding_confirmation = None;
                            if let Some(last) = self.chat_messages.last() {
                                if last.role == ChatRole::User { self.chat_messages.pop(); }
                            }
                        }
                    });
                });
            ui.add_space(4.0);
        }

        // ── Coding mini status bar ────────────────────────────────────────────
        if !self.coding_console.task.is_empty() {
            let is_done = self.coding_console.status.contains("完成") || self.coding_console.status.contains("✅");
            let is_err  = self.coding_console.status.contains("錯誤") || self.coding_console.status.contains("Error");
            let bar_color = if is_done { Color32::from_rgb(100, 220, 100) }
                else if is_err { Color32::from_rgb(220, 80, 80) }
                else { Color32::YELLOW };
            egui::Frame::new()
                .fill(Color32::from_rgb(20, 30, 40))
                .inner_margin(egui::Margin::symmetric(8, 4))
                .corner_radius(4.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(bar_color, "⚙");
                        ui.small(&self.coding_console.status);
                        if !self.coding_console.files_modified.is_empty() {
                            ui.small(format!("· 📁 {}", self.coding_console.files_modified.join(", ")));
                        }
                    });
                });
            ui.add_space(2.0);
        }

        // ── Chat message history ──────────────────────────────────────────────
        let available = ui.available_height();
        ScrollArea::vertical()
            .id_salt("ws_chat")
            .max_height(available.max(100.0))
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                if self.chat_messages.is_empty() {
                    ui.colored_label(Color32::GRAY, "輸入上方文字框開始對話…");
                }
                for msg in &self.chat_messages {
                    let (bg, label, text_color) = match msg.role {
                        ChatRole::User => (Color32::from_rgb(40, 60, 100), "你", Color32::WHITE),
                        ChatRole::Assistant => (
                            Color32::from_rgb(45, 55, 45), "Sirin",
                            Color32::from_rgb(200, 240, 200),
                        ),
                    };
                    egui::Frame::new()
                        .fill(bg)
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .corner_radius(6.0)
                        .show(ui, |ui| {
                            ui.colored_label(Color32::GRAY, label);
                            ui.colored_label(text_color, &msg.text);
                        });
                    ui.add_space(4.0);
                }
                if self.chat_pending {
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
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                }
            });
    }

    /// 活動 sub-tab: task log filtered to a specific agent.
    fn show_tasks_for_agent(&mut self, ui: &mut egui::Ui, agent_name: &str) {
        let running_count = self.tasks.iter()
            .filter(|t| t.persona == agent_name && matches!(t.status.as_deref(), Some("PENDING") | Some("RUNNING") | Some("FOLLOWING")))
            .count();
        let attention_count = self.tasks.iter()
            .filter(|t| t.persona == agent_name && matches!(t.status.as_deref(), Some("FOLLOWUP_NEEDED") | Some("FAILED") | Some("ERROR") | Some("ROLLBACK")))
            .count();
        let done_count = self.tasks.iter()
            .filter(|t| t.persona == agent_name && t.status.as_deref() == Some("DONE"))
            .count();

        ui.horizontal(|ui| {
            ui.label(RichText::new("活動記錄").strong());
            ui.separator();
            ui.selectable_value(&mut self.task_filter, TaskFilter::All, "全部");
            ui.selectable_value(&mut self.task_filter, TaskFilter::Running, "進行中");
            ui.selectable_value(&mut self.task_filter, TaskFilter::Done, "完成");
            ui.selectable_value(&mut self.task_filter, TaskFilter::Failed, "需處理");
            ui.separator();
            ui.small(format!("⏳ {}", running_count));
            ui.small(format!("⚠️ {}", attention_count));
            ui.small(format!("✅ {}", done_count));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let total = self.tasks.iter().filter(|t| t.persona == agent_name).count();
                ui.small(format!("{total} 筆"));
                if ui.button(RichText::new("🗑 清除").small().color(Color32::from_rgb(200, 80, 80)))
                    .on_hover_text("清除所有活動紀錄").clicked()
                {
                    if let Err(e) = self.tracker.clear() { eprintln!("[ui] clear task log: {e}"); }
                    self.tasks.clear();
                }
            });
        });
        ui.separator();

        let filter = self.task_filter;
        let row_height = 18.0;
        let col_widths = [120.0_f32, 230.0, 50.0];
        ui.horizontal(|ui| {
            ui.add_sized([col_widths[0], row_height], egui::Label::new(RichText::new("時間").strong().small()));
            ui.add_sized([col_widths[1], row_height], egui::Label::new(RichText::new("事件").strong().small()));
            ui.label(RichText::new("狀態").strong().small());
        });
        ui.separator();

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for task in &self.tasks {
                if task.persona != agent_name { continue; }
                let status = task.status.as_deref().unwrap_or("—");
                let passes = match filter {
                    TaskFilter::All => true,
                    TaskFilter::Running => matches!(status, "PENDING" | "RUNNING" | "FOLLOWING"),
                    TaskFilter::Done => status == "DONE",
                    TaskFilter::Failed => matches!(status, "FAILED" | "ERROR" | "FOLLOWUP_NEEDED" | "ROLLBACK"),
                };
                if !passes { continue; }

                let ts = task.timestamp.get(..19).unwrap_or(&task.timestamp);
                let is_summary = task.event == "research_summary_ready";
                let (badge_text, badge_fg, badge_bg) =
                    task_status_badge(status, task.reason.as_deref(), is_summary);

                ui.horizontal(|ui| {
                    ui.add_sized([col_widths[0], row_height],
                        egui::Label::new(RichText::new(ts).monospace().small()));
                    let preview = task.message_preview.as_deref()
                        .or(task.reason.as_deref())
                        .unwrap_or(&task.event);
                    let event_label = if is_summary { "🧠 research_summary" }
                        else if task.event == "adk_coding_fail_fast" { "⚠ coding_fail_fast" }
                        else if task.event == "adk_coding_rollback" { "↩ coding_rollback" }
                        else if task.event == "adk_coding_agent_done" { "⚙ coding_done" }
                        else { &task.event };
                    let label_text = format!("{} — {}", event_label, preview.chars().take(80).collect::<String>());
                    ui.add_sized([col_widths[1], row_height], egui::Label::new(&label_text).truncate());
                    egui::Frame::new()
                        .fill(badge_bg)
                        .stroke(egui::Stroke::new(1.0, badge_fg))
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .corner_radius(6.0)
                        .show(ui, |ui| {
                            ui.label(RichText::new(badge_text).small().strong().color(badge_fg));
                        });
                });
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

    /// 調研 sub-tab: research log + pending persona objective gate.
    fn show_research_workspace(&mut self, ui: &mut egui::Ui) {
        // Persona objective gate
        if let Some(proposed) = self.pending_objectives.clone() {
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(40, 35, 15))
                .show(ui, |ui| {
                    ui.label(RichText::new("⚠ AI 提議更新 Persona 目標（需您確認）")
                        .color(Color32::YELLOW).strong());
                    for (i, obj) in proposed.iter().enumerate() {
                        ui.label(format!("  {}. {}", i + 1, obj));
                    }
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("✅ 套用").color(Color32::from_rgb(100, 220, 100))).clicked() {
                            match crate::persona::Persona::load() {
                                Ok(mut p) => {
                                    p.objectives = proposed.clone();
                                    match p.save() {
                                        Ok(()) => { self.research_msg = "Persona 目標已更新".to_string(); }
                                        Err(e) => { self.research_msg = format!("儲存失敗: {e}"); }
                                    }
                                }
                                Err(e) => { self.research_msg = format!("載入 Persona 失敗: {e}"); }
                            }
                            self.pending_objectives = None;
                        }
                        if ui.button(RichText::new("❌ 拒絕").color(Color32::from_rgb(220, 80, 80))).clicked() {
                            self.pending_objectives = None;
                            self.research_msg = "已拒絕 AI 目標提議".to_string();
                        }
                    });
                });
            ui.separator();
        }
        if !self.research_msg.is_empty() {
            ui.colored_label(Color32::from_rgb(100, 220, 100), &self.research_msg);
            ui.add_space(2.0);
        }

        ui.horizontal(|ui| {
            ui.label(format!("{} 筆調研記錄", self.research_tasks.len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("🗑 清除").small().color(Color32::from_rgb(200, 80, 80)))
                    .on_hover_text("清除所有調研紀錄").clicked()
                {
                    if let Err(e) = crate::researcher::clear_research() { eprintln!("[ui] clear research: {e}"); }
                    self.research_tasks.clear();
                    self.research_expanded.clear();
                }
            });
        });

        let mut toggle_expand: Option<String> = None;
        ScrollArea::vertical().id_salt("ws_research").auto_shrink(false).show(ui, |ui| {
            for task in &self.research_tasks {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (color, label) = match task.status {
                            crate::researcher::ResearchStatus::Done => (Color32::from_rgb(100, 220, 100), "完成"),
                            crate::researcher::ResearchStatus::Running => (Color32::YELLOW, "進行中"),
                            crate::researcher::ResearchStatus::Failed => (Color32::from_rgb(220, 80, 80), "失敗"),
                        };
                        ui.colored_label(color, label);
                        ui.strong(&task.topic);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.small(task.started_at.get(..10).unwrap_or(&task.started_at));
                        });
                    });
                    if let Some(ref report) = task.final_report {
                        let is_expanded = self.research_expanded.contains(&task.id);
                        if is_expanded {
                            ScrollArea::vertical().id_salt(format!("rpt_{}", task.id)).max_height(280.0).show(ui, |ui| {
                                ui.label(report.as_str() as &str);
                            });
                            if ui.small_button("▲ 收起").clicked() { toggle_expand = Some(task.id.clone()); }
                        } else {
                            let preview: String = report.chars().take(200).collect();
                            ui.small(format!("{}…", preview.trim_end()));
                            if ui.small_button("▼ 展開").clicked() { toggle_expand = Some(task.id.clone()); }
                        }
                    } else if task.status == crate::researcher::ResearchStatus::Failed {
                        if let Some(err_step) = task.steps.iter().find(|s| s.phase == "error") {
                            ui.colored_label(Color32::from_rgb(220, 100, 100),
                                format!("❌ {}", err_step.output.chars().take(120).collect::<String>()));
                        }
                    }
                });
            }
        });
        if let Some(id) = toggle_expand {
            if self.research_expanded.contains(&id) { self.research_expanded.remove(&id); }
            else { self.research_expanded.insert(id); }
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
                                            tg_status_badge(ui, st);
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
                let st = a.status();
                tg_status_badge(ui, &st);
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

/// Renders a compact Telegram connection status icon (✈●/✈⚠/✈✗/✈○).
fn tg_status_badge(ui: &mut egui::Ui, status: &crate::telegram_auth::TelegramStatus) {
    use crate::telegram_auth::TelegramStatus;
    match status {
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
