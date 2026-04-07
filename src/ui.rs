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
    Tasks,
    Research,
    Chat,
    Settings,
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

    // Log panel
    log_visible: bool,

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

    // Settings tab
    /// Working copy of persona being edited (loaded lazily).
    settings_persona: Option<crate::persona::Persona>,
    /// Status message shown after save / error.
    settings_msg: String,
    settings_msg_at: Option<std::time::Instant>,
    /// Input buffer for adding a new objective.
    settings_new_objective: String,
    /// Input buffer for adding a new allowed command.
    settings_new_command: String,
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

    pub fn new(tracker: TaskTracker, tg_auth: TelegramAuthState, rt: Handle) -> Self {
        let _ = ensure_codebase_index();

        let (chat_tx, chat_rx) = std::sync::mpsc::sync_channel(8);
        let (coding_tx, coding_rx) = std::sync::mpsc::sync_channel(4);
        let mut app = Self {
            tracker,
            tg_auth,
            rt,
            tab: Tab::Tasks,
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
            log_visible: true,
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            event_rx: crate::events::subscribe(),
            debug_panel_open: false,
            task_filter: TaskFilter::All,
            research_expanded: std::collections::HashSet::new(),
            coding_done_at: None,
            research_msg_at: None,
            storage: crate::memory::StorageUsage::default(),
            settings_persona: None,
            settings_msg: String::new(),
            settings_msg_at: None,
            settings_new_objective: String::new(),
            settings_new_command: String::new(),
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
                Ok(_) => {} // other events — ignored in UI for now
                Err(broadcast::error::TryRecvError::Lagged(_)) => {} // skip lagged events
                Err(_) => break, // Empty or Closed
            }
        }

        // ── Top panel (tabs + refresh) ────────────────────────────────────────
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Sirin");
                ui.separator();
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳").on_hover_text("立即刷新").clicked() {
                        self.refresh();
                    }
                    let secs = self.last_refresh.elapsed().as_secs();
                    ui.small(format!("{secs}s 前"));
                    ui.separator();
                    let log_label = if self.log_visible {
                        "📋 隱藏 Log"
                    } else {
                        "📋 顯示 Log"
                    };
                    if ui.small_button(log_label).clicked() {
                        self.log_visible = !self.log_visible;
                    }
                });
            });
        });

        // ── Bottom log panel ──────────────────────────────────────────────────
        if self.log_visible {
            egui::TopBottomPanel::bottom("log_panel")
                .resizable(true)
                .min_height(80.0)
                .max_height(300.0)
                .default_height(140.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Log");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("清除").clicked() {
                                log_buffer::clear();
                            }
                            if ui.small_button("複製全部").clicked() {
                                ui.ctx().copy_text(log_buffer::snapshot_text(300));
                            }
                        });
                    });
                    ui.separator();
                    ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            for line in log_buffer::recent(200) {
                                let color = if line.contains("error")
                                    || line.contains("Error")
                                    || line.contains("failed")
                                    || line.contains("Failed")
                                {
                                    Color32::from_rgb(220, 100, 100)
                                } else if line.contains("[telegram]") {
                                    Color32::from_rgb(100, 180, 255)
                                } else if line.contains("[researcher]") {
                                    Color32::from_rgb(150, 220, 150)
                                } else if line.contains("[followup]") {
                                    Color32::from_rgb(220, 180, 100)
                                } else {
                                    Color32::GRAY
                                };
                                ui.colored_label(
                                    color,
                                    egui::RichText::new(&line).monospace().small(),
                                );
                            }
                        });
                });
        }

        // ── Central panel ─────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Tasks => self.show_tasks(ui),
            Tab::Research => self.show_research(ui),
            Tab::Chat => self.show_chat(ui),
            Tab::Settings => self.show_settings(ui),
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
                // Apply filter.
                let passes = match filter {
                    TaskFilter::All => true,
                    TaskFilter::Running => matches!(status, "PENDING" | "RUNNING" | "FOLLOWING"),
                    TaskFilter::Done => status == "DONE",
                    TaskFilter::Failed => {
                        matches!(status, "FAILED" | "ERROR" | "FOLLOWUP_NEEDED" | "ROLLBACK")
                    }
                };
                if !passes {
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
                                });
                            run_chat_and_send(request, plan_update, user_text_spawn, tx).await;
                        }
                    });
                }
            } // end else (not /skill command)
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        use crate::persona::{Persona, ProfessionalTone};

        // Lazy-load the persona into the working copy when the tab is first opened.
        if self.settings_persona.is_none() {
            self.settings_persona = Persona::load().ok();
        }

        if self.settings_persona.is_none() {
            ui.colored_label(Color32::from_rgb(220, 80, 80), "無法載入 config/persona.yaml");
            if ui.button("重試").clicked() {
                self.settings_persona = Persona::load().ok();
            }
            return;
        }

        // ── Action flags (set inside scroll closure, executed after) ──────────
        let mut do_save = false;
        let mut do_reload = false;

        // Move input buffers out of self so the closure can capture both `p`
        // (from self.settings_persona) and these buffers without conflicting borrows.
        let mut new_obj = std::mem::take(&mut self.settings_new_objective);
        let mut new_cmd = std::mem::take(&mut self.settings_new_command);
        let settings_msg = self.settings_msg.clone();

        // Telegram state extracted before closure (tg_auth is Arc-Clone, cheap).
        let mut tg_code = std::mem::take(&mut self.tg_code);
        let mut tg_password = std::mem::take(&mut self.tg_password);
        let tg_msg = self.tg_msg.clone();
        let tg_auth = self.tg_auth.clone();
        let mut tg_msg_update: Option<String> = None;

        let p = self.settings_persona.as_mut().unwrap();

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            // ── 身份 ─────────────────────────────────────────────────────────
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("身份").strong());
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("名稱：");
                    ui.text_edit_singleline(&mut p.identity.name);
                });
                ui.horizontal(|ui| {
                    ui.label("語氣：");
                    ui.selectable_value(
                        &mut p.identity.professional_tone,
                        ProfessionalTone::Brief,
                        "Brief（簡潔）",
                    );
                    ui.selectable_value(
                        &mut p.identity.professional_tone,
                        ProfessionalTone::Detailed,
                        "Detailed（詳細）",
                    );
                    ui.selectable_value(
                        &mut p.identity.professional_tone,
                        ProfessionalTone::Casual,
                        "Casual（輕鬆）",
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("描述：");
                    ui.text_edit_singleline(&mut p.description);
                });
            });

            ui.add_space(6.0);

            // ── 回覆風格 ──────────────────────────────────────────────────────
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("回覆風格").strong());
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("語音風格：");
                    ui.text_edit_singleline(&mut p.response_style.voice);
                });
                ui.horizontal(|ui| {
                    ui.label("確認前綴：");
                    ui.text_edit_singleline(&mut p.response_style.ack_prefix);
                });
                ui.horizontal(|ui| {
                    ui.label("合規提示：");
                    ui.text_edit_singleline(&mut p.response_style.compliance_line);
                });
            });

            ui.add_space(6.0);

            // ── 目標 ─────────────────────────────────────────────────────────
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("Persona 目標").strong());
                ui.separator();
                let mut remove_idx: Option<usize> = None;
                for (i, obj) in p.objectives.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}.  {obj}", i + 1));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button(RichText::new("✕").color(Color32::from_rgb(200, 80, 80)))
                                .clicked()
                            {
                                remove_idx = Some(i);
                            }
                        });
                    });
                }
                if let Some(idx) = remove_idx {
                    p.objectives.remove(idx);
                }
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut new_obj);
                    if ui.button("＋ 新增目標").clicked() && !new_obj.trim().is_empty() {
                        p.objectives.push(new_obj.trim().to_string());
                        new_obj.clear();
                    }
                });
            });

            ui.add_space(6.0);

            // ── ROI 閾值 ──────────────────────────────────────────────────────
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("ROI 閾值").strong());
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("最低通知金額（USD）：");
                    ui.add(
                        egui::DragValue::new(&mut p.roi_thresholds.min_usd_to_notify)
                            .range(0.0..=10000.0)
                            .speed(0.5),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("最低遠端 LLM 調用金額（USD）：");
                    ui.add(
                        egui::DragValue::new(&mut p.roi_thresholds.min_usd_to_call_remote_llm)
                            .range(0.0..=10000.0)
                            .speed(1.0),
                    );
                });
            });

            ui.add_space(6.0);

            // ── Coding Agent ──────────────────────────────────────────────────
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("Coding Agent").strong());
                ui.separator();
                ui.checkbox(&mut p.coding_agent.enabled, "啟用 Coding Agent");
                ui.horizontal(|ui| {
                    ui.label("專案根目錄：");
                    ui.text_edit_singleline(&mut p.coding_agent.project_root);
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut p.coding_agent.auto_approve_reads, "自動允許讀取操作");
                    ui.checkbox(
                        &mut p.coding_agent.auto_approve_writes,
                        "自動允許寫入操作（不詢問確認）",
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("最大迭代次數：");
                    ui.add(egui::DragValue::new(&mut p.coding_agent.max_iterations).range(1..=50));
                });
                ui.horizontal(|ui| {
                    ui.label("最大寫入位元組（bytes）：");
                    ui.add(
                        egui::DragValue::new(&mut p.coding_agent.max_file_write_bytes)
                            .range(1024..=10485760)
                            .speed(1024.0),
                    );
                });
                ui.label(RichText::new("允許執行的指令：").small());
                let mut remove_cmd: Option<usize> = None;
                for (i, cmd) in p.coding_agent.allowed_commands.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.monospace(cmd);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button(RichText::new("✕").color(Color32::from_rgb(200, 80, 80)))
                                .clicked()
                            {
                                remove_cmd = Some(i);
                            }
                        });
                    });
                }
                if let Some(idx) = remove_cmd {
                    p.coding_agent.allowed_commands.remove(idx);
                }
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut new_cmd);
                    if ui.button("＋ 新增指令").clicked() && !new_cmd.trim().is_empty() {
                        p.coding_agent.allowed_commands.push(new_cmd.trim().to_string());
                        new_cmd.clear();
                    }
                });
            });

            ui.add_space(10.0);

            // ── Telegram ─────────────────────────────────────────────────────
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("Telegram").strong());
                ui.separator();

                // Connection status
                let tg_status = tg_auth.status();
                let (status_color, status_label, status_detail) = match &tg_status {
                    TelegramStatus::Connected =>
                        (Color32::from_rgb(100, 220, 100), "✈ 已連線", None),
                    TelegramStatus::Disconnected { reason } =>
                        (Color32::GRAY, "○ 未連線", Some(reason.clone())),
                    TelegramStatus::CodeRequired =>
                        (Color32::YELLOW, "⏳ 需要驗證碼", None),
                    TelegramStatus::PasswordRequired { hint } =>
                        (Color32::YELLOW, "⏳ 需要 2FA 密碼", Some(hint.clone())),
                    TelegramStatus::Error { message } =>
                        (Color32::from_rgb(220, 80, 80), "❌ 錯誤", Some(message.clone())),
                };
                ui.horizontal(|ui| {
                    ui.colored_label(status_color, status_label);
                    if let Some(d) = status_detail {
                        ui.small(&d);
                    }
                });

                // Auth input forms
                match &tg_status {
                    TelegramStatus::CodeRequired => {
                        ui.add_space(4.0);
                        ui.label("輸入 Telegram 驗證碼：");
                        let submitted = ui.horizontal(|ui| {
                            let r = ui.text_edit_singleline(&mut tg_code);
                            let btn = ui.button("提交");
                            (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                || btn.clicked()
                        }).inner;
                        if submitted && !tg_code.trim().is_empty() {
                            tg_auth.submit_code(tg_code.trim().to_string());
                            tg_code.clear();
                            tg_msg_update = Some("驗證碼已提交".to_string());
                        }
                    }
                    TelegramStatus::PasswordRequired { hint } => {
                        ui.add_space(4.0);
                        ui.label(format!("輸入 2FA 密碼（提示：{hint}）："));
                        let submitted = ui.horizontal(|ui| {
                            let r = ui.add(
                                TextEdit::singleline(&mut tg_password).password(true),
                            );
                            let btn = ui.button("提交");
                            (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                || btn.clicked()
                        }).inner;
                        if submitted && !tg_password.trim().is_empty() {
                            tg_auth.submit_password(tg_password.clone());
                            tg_password.clear();
                            tg_msg_update = Some("密碼已提交".to_string());
                        }
                    }
                    _ => {}
                }

                if !tg_msg.is_empty() {
                    ui.colored_label(Color32::from_rgb(100, 220, 100), &tg_msg);
                }

                ui.add_space(4.0);

                // Env var status (read-only)
                let auto_reply = std::env::var("TG_AUTO_REPLY").unwrap_or_default();
                let auto_priv  = std::env::var("TG_REPLY_PRIVATE").unwrap_or_default();
                let auto_grp   = std::env::var("TG_REPLY_GROUPS").unwrap_or_default();
                let on  = Color32::from_rgb(100, 220, 100);
                let off = Color32::from_rgb(140, 140, 140);
                ui.horizontal(|ui| {
                    ui.colored_label(
                        if auto_reply == "true" { on } else { off },
                        if auto_reply == "true" { "● 自動回覆：開" } else { "○ 自動回覆：關" },
                    );
                    ui.separator();
                    ui.colored_label(
                        if auto_priv == "true" { on } else { off },
                        if auto_priv == "true" { "私訊 ✓" } else { "私訊 ✗" },
                    );
                    ui.colored_label(
                        if auto_grp == "true" { on } else { off },
                        if auto_grp == "true" { "群組 ✓" } else { "群組 ✗" },
                    );
                });

                // .env not configured guide
                let has_creds = std::env::var("TG_API_ID").is_ok()
                    && std::env::var("TG_API_HASH").is_ok();
                if !has_creds {
                    ui.add_space(4.0);
                    ui.colored_label(Color32::YELLOW, "⚙ 尚未設定 TG_API_ID / TG_API_HASH");
                    ui.label("請在 .env 加入：");
                    let snippet = "TG_API_ID=12345678\nTG_API_HASH=your_hash\nTG_PHONE=+886...";
                    let mut s = snippet.to_string();
                    ui.add(TextEdit::multiline(&mut s).code_editor().desired_rows(3).desired_width(f32::INFINITY));
                    if ui.small_button("📋 複製").clicked() {
                        ui.ctx().copy_text(snippet.to_string());
                    }
                }

                ui.add_space(4.0);

                // Recent Telegram activity log
                ui.label(RichText::new("近期活動").small().strong());
                let tg_lines: Vec<String> = log_buffer::recent(300)
                    .into_iter()
                    .filter(|l| l.contains("[telegram]") || l.contains("[tg]"))
                    .take(40)
                    .collect();
                if tg_lines.is_empty() {
                    ui.colored_label(Color32::GRAY, "（尚無 Telegram 活動記錄）");
                } else {
                    ScrollArea::vertical()
                        .id_salt("tg_log_settings")
                        .stick_to_bottom(true)
                        .max_height(180.0)
                        .show(ui, |ui| {
                            for line in &tg_lines {
                                let color = if line.contains("error") || line.contains("Error") || line.contains("failed") {
                                    Color32::from_rgb(220, 100, 100)
                                } else if line.contains("reply") || line.contains("sent") {
                                    Color32::from_rgb(150, 220, 150)
                                } else {
                                    Color32::from_rgb(100, 180, 255)
                                };
                                ui.colored_label(color, RichText::new(line).monospace().small());
                            }
                        });
                }
            });

            ui.add_space(10.0);

            // ── 動作按鈕（flags — 實際執行在 closure 外面）────────────────────
            ui.horizontal(|ui| {
                if ui.button(RichText::new("💾 儲存設定").strong()).clicked() {
                    do_save = true;
                }
                if ui
                    .button("↺ 重新載入")
                    .on_hover_text("丟棄未儲存的變更，重新從磁碟讀取")
                    .clicked()
                {
                    do_reload = true;
                }
                if !settings_msg.is_empty() {
                    let color = if settings_msg.starts_with('❌') {
                        Color32::from_rgb(220, 80, 80)
                    } else {
                        Color32::from_rgb(100, 220, 100)
                    };
                    ui.colored_label(color, &settings_msg);
                }
            });

            ui.add_space(6.0);
            ui.separator();
            ui.label(
                RichText::new("⚠ LLM 後端（端點/模型）等環境變數需編輯 .env 檔案後重啟生效。")
                    .small()
                    .color(Color32::GRAY),
            );
        });

        // Write all extracted buffers back after closure releases the borrow on p.
        self.settings_new_objective = new_obj;
        self.settings_new_command = new_cmd;
        self.tg_code = tg_code;
        self.tg_password = tg_password;
        if let Some(msg) = tg_msg_update {
            self.tg_msg = msg;
        }

        // ── Execute deferred actions ──────────────────────────────────────────
        if do_save {
            let p = self.settings_persona.as_ref().unwrap();
            match p.save() {
                Ok(()) => {
                    self.auto_approve_writes = p.coding_agent.auto_approve_writes;
                    self.settings_msg = "✅ 設定已儲存並立即生效".to_string();
                }
                Err(e) => {
                    self.settings_msg = format!("❌ 儲存失敗：{e}");
                }
            }
            self.settings_msg_at = Some(std::time::Instant::now());
        }

        if do_reload {
            match Persona::load() {
                Ok(fresh) => {
                    self.settings_persona = Some(fresh);
                    self.settings_msg = "已重新載入".to_string();
                }
                Err(e) => {
                    self.settings_msg = format!("❌ 載入失敗：{e}");
                }
            }
            self.settings_msg_at = Some(std::time::Instant::now());
        }
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

    let _ = crate::memory::append_context(&user_text, &response.reply, Some(0));
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
