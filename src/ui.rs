//! Native egui/eframe UI for Sirin.
//!
//! Runs on the main thread. Background Tokio tasks (Telegram listener,
//! follow-up worker) communicate via the same shared-state structs they
//! always have — no IPC layer needed.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, RichText, ScrollArea, TextEdit};
use tokio::runtime::Handle;

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
    Telegram,
    Chat,
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
        if !self.steps.is_empty() {
            lines.push(format!("Steps: {}", self.steps.join(" → ")));
        }
        if !self.recommended_skills.is_empty() {
            lines.push(format!("Recommended Skills: {}", self.recommended_skills.join(", ")));
        }
        if !self.tools.is_empty() {
            lines.push(format!("Tools: {}", self.tools.join(", ")));
        }
        if !self.trace.is_empty() {
            lines.push("Trace:".to_string());
            lines.extend(self.trace.iter().map(|item| format!("- {item}")));
        }
        if !self.latest_task_summary.is_empty() {
            lines.push(format!("Latest Research Summary: {}", self.latest_task_summary));
        }

        lines.join("\n")
    }
}

#[derive(Clone, Default)]
struct CodingConsoleState {
    status: String,
    task: String,
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
        if !self.files_modified.is_empty() {
            lines.push(format!("Files modified: {}", self.files_modified.join(", ")));
        }
        if !self.outcome.is_empty() {
            lines.push(format!("Outcome: {}", self.outcome));
        }
        if self.verified {
            lines.push("✅ cargo check: passed".to_string());
        }
        if let Some(ref vout) = self.verification_output {
            lines.push(format!("Verification: {}", vout.chars().take(200).collect::<String>()));
        }
        if !self.trace.is_empty() {
            lines.push("Trace:".to_string());
            lines.extend(self.trace.iter().map(|s| format!("  {s}")));
        }
        if let Some(ref diff) = self.diff {
            lines.push(format!("Diff preview:\n{}", diff.chars().take(400).collect::<String>()));
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

fn build_console_log_bundle(console: &AgentConsoleState, messages: &[ChatMessage], log_lines: usize) -> String {
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

    // Log panel
    log_visible: bool,

    last_refresh: std::time::Instant,
}

impl SirinApp {
    /// Load a CJK-capable font from the Windows system font directory.
    /// Falls back silently if the font file cannot be read.
    pub fn setup_fonts(ctx: &egui::Context) {
        let font_path = std::path::Path::new("C:/Windows/Fonts/msjh.ttc"); // Microsoft JhengHei (繁中)
        let fallback = std::path::Path::new("C:/Windows/Fonts/msyh.ttc");  // Microsoft YaHei (簡中)

        let font_data = if font_path.exists() {
            std::fs::read(font_path).ok()
        } else if fallback.exists() {
            std::fs::read(fallback).ok()
        } else {
            None
        };

        if let Some(bytes) = font_data {
            let mut fonts = FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_owned(),
                FontData::from_owned(bytes).into(),
            );
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
            log_visible: true,
            last_refresh: std::time::Instant::now()
                - std::time::Duration::from_secs(60),
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

                if let Some(summary_entry) = self.tasks.iter().find(|task| task.event == "research_summary_ready") {
                    self.agent_console.latest_task_summary = summary_entry
                        .reason
                        .as_deref()
                        .unwrap_or_default()
                        .chars()
                        .take(220)
                        .collect();
                }
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
                self.coding_console.files_modified = resp.files_modified.clone();
                self.coding_console.trace = resp.trace.clone();
                self.coding_console.diff = resp.diff.clone();
                self.coding_console.verified = resp.verified;
                self.coding_console.verification_output = resp.verification_output.clone();
                self.coding_console.dry_run = resp.dry_run;
                self.coding_console.status = if resp.verified {
                    "✅ 完成（已驗證）".to_string()
                } else {
                    "完成".to_string()
                };
                // Push the outcome as an assistant message in the chat.
                let summary = format!(
                    "**[Coding Agent]** {}\n\n{}{}",
                    resp.outcome,
                    if resp.files_modified.is_empty() { String::new() }
                        else { format!("📁 已修改：{}\n", resp.files_modified.join(", ")) },
                    if resp.dry_run { "（Dry-run 模式：檔案未寫入）" } else { "" }
                );
                if let Some(last) = self.chat_messages.last_mut() {
                    if last.role == ChatRole::Assistant {
                        last.text = summary;
                    } else {
                        self.chat_messages.push(ChatMessage { role: ChatRole::Assistant, text: summary });
                    }
                } else {
                    self.chat_messages.push(ChatMessage { role: ChatRole::Assistant, text: summary });
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

        // ── Top panel (tabs + refresh) ────────────────────────────────────────
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Sirin");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Tasks, "📋 任務板");
                ui.selectable_value(&mut self.tab, Tab::Research, "🔬 調研");
                ui.selectable_value(&mut self.tab, Tab::Telegram, "✈ Telegram");
                ui.selectable_value(&mut self.tab, Tab::Chat, "💬 對話");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳").on_hover_text("立即刷新").clicked() {
                        self.refresh();
                    }
                    let secs = self.last_refresh.elapsed().as_secs();
                    ui.small(format!("{secs}s 前"));
                    ui.separator();
                    let log_label = if self.log_visible { "📋 隱藏 Log" } else { "📋 顯示 Log" };
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
                                let color = if line.contains("error") || line.contains("Error") || line.contains("failed") || line.contains("Failed") {
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
                                ui.colored_label(color, egui::RichText::new(&line).monospace().small());
                            }
                        });
                });
        }

        // ── Central panel ─────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Tasks => self.show_tasks(ui),
            Tab::Research => self.show_research(ui),
            Tab::Telegram => self.show_telegram(ui),
            Tab::Chat => self.show_chat(ui),
        });
    }
}

// ── Tab rendering ─────────────────────────────────────────────────────────────

impl SirinApp {
    fn show_tasks(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("共 {} 筆任務（最近 200 筆，不含 heartbeat）", self.tasks.len()));
        ui.separator();

        let row_height = 18.0;
        let col_widths = [130.0_f32, 200.0, 40.0];

        // Header
        ui.horizontal(|ui| {
            ui.add_sized([col_widths[0], row_height], egui::Label::new(RichText::new("時間").strong()));
            ui.add_sized([col_widths[1], row_height], egui::Label::new(RichText::new("事件").strong()));
            ui.label(RichText::new("狀態").strong());
        });
        ui.separator();

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for task in &self.tasks {
                let ts = task.timestamp.get(..19).unwrap_or(&task.timestamp);
                let status = task.status.as_deref().unwrap_or("—");
                let is_summary = task.event == "research_summary_ready";
                let status_color = match status {
                    "DONE" if is_summary => Color32::from_rgb(120, 210, 255),
                    "DONE" => Color32::from_rgb(100, 220, 100),
                    "PENDING" => Color32::from_rgb(255, 200, 60),
                    "FAILED" | "ERROR" => Color32::from_rgb(220, 80, 80),
                    _ => Color32::GRAY,
                };

                ui.horizontal(|ui| {
                    ui.add_sized(
                        [col_widths[0], row_height],
                        egui::Label::new(egui::RichText::new(ts).monospace().small()),
                    );
                    let preview = task
                        .message_preview
                        .as_deref()
                        .or(task.reason.as_deref())
                        .unwrap_or(&task.event);
                    let event_label = if is_summary { "research_summary" } else { &task.event };
                    let label_text = format!("{} — {}", event_label,
                        preview.chars().take(80).collect::<String>());
                    ui.add_sized(
                        [col_widths[1], row_height],
                        egui::Label::new(&label_text).truncate(),
                    );
                    ui.colored_label(status_color, status);
                });

                if is_summary {
                    if let Some(reason) = task.reason.as_deref() {
                        let summary_preview: String = reason.chars().take(140).collect();
                        ui.add_space(2.0);
                        ui.small(format!("   ↳ {}", summary_preview));
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
                            .button(RichText::new("✅ 套用").color(Color32::from_rgb(100, 220, 100)))
                            .clicked()
                        {
                            match crate::persona::Persona::load() {
                                Ok(mut p) => {
                                    p.objectives = proposed.clone();
                                    match p.save() {
                                        Ok(()) => {
                                            self.research_msg =
                                                "Persona 目標已更新".to_string();
                                        }
                                        Err(e) => {
                                            self.research_msg =
                                                format!("儲存失敗: {e}");
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
                        let task = crate::agents::research_agent::run_research_via_adk(topic, url).await;
                        eprintln!("[ui] research '{}' → {:?}", task.id, task.status);
                    });
                    self.research_msg = format!("已啟動：{}", self.research_topic.trim());
                    self.research_topic.clear();
                    self.research_url.clear();
                }
                if !self.research_msg.is_empty() {
                    ui.colored_label(Color32::from_rgb(100, 220, 100), &self.research_msg);
                }
            });
        });

        ui.separator();
        ui.label(format!("{} 筆調研記錄", self.research_tasks.len()));

        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for task in &self.research_tasks {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (color, label) = match task.status {
                            ResearchStatus::Done => (Color32::from_rgb(100, 220, 100), "完成"),
                            ResearchStatus::Running => (Color32::YELLOW, "進行中"),
                            ResearchStatus::Failed => (Color32::from_rgb(220, 80, 80), "失敗"),
                        };
                        ui.colored_label(color, label);
                        ui.strong(&task.topic);
                        if let Some(ref url) = task.url {
                            ui.small(url);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.small(task.started_at.get(..10).unwrap_or(&task.started_at));
                        });
                    });
                    if let Some(ref report) = task.final_report {
                        let preview: String = report.chars().take(300).collect();
                        ui.label(preview);
                    }
                    ui.small(format!("{} 個步驟", task.steps.len()));
                });
            }
        });
    }

    fn show_telegram(&mut self, ui: &mut egui::Ui) {
        let status = self.tg_auth.status();

        // ── Setup guide when env vars are not configured ──────────────────────
        let has_api_id = std::env::var("TG_API_ID").is_ok();
        let has_api_hash = std::env::var("TG_API_HASH").is_ok();
        if !has_api_id || !has_api_hash {
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(25, 30, 45))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("⚙ 尚未設定 Telegram API 憑證")
                            .color(Color32::YELLOW)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label("請在專案根目錄建立 .env 檔案，填入以下設定：");
                    ui.add_space(4.0);
                    let guide = "# 從 https://my.telegram.org 取得\n\
TG_API_ID=12345678\n\
TG_API_HASH=your_api_hash_here\n\
TG_PHONE=+886912345678    # 選填：自動登入用\n\
\n\
# 回覆設定\n\
TG_AUTO_REPLY=true        # 啟用 AI 自動回覆\n\
TG_REPLY_PRIVATE=true     # 回覆私訊\n\
TG_REPLY_GROUPS=false     # 是否回覆群組\n\
TG_GROUP_IDS=             # 選填：只監控特定群組 ID（逗號分隔）\n\
\n\
# 除錯\n\
TG_DEBUG_UPDATES=false";
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        ui.add(
                            TextEdit::multiline(&mut guide.to_string().as_str())
                                .code_editor()
                                .desired_rows(8)
                                .desired_width(f32::INFINITY),
                        );
                    });
                    ui.add_space(4.0);
                    if ui.button("📋 複製 .env 範本").clicked() {
                        ui.ctx().copy_text(guide.to_string());
                    }
                    ui.add_space(2.0);
                    ui.small("設定完成後重新啟動應用程式，Telegram 監聽器將自動連線。");
                });
            ui.separator();
        }

        // Status badge
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("Telegram 連線狀態").strong());
            let (color, label, detail) = match &status {
                TelegramStatus::Connected => {
                    (Color32::from_rgb(100, 220, 100), "已連線", None)
                }
                TelegramStatus::Disconnected { reason } => {
                    (Color32::GRAY, "未連線", Some(reason.as_str()))
                }
                TelegramStatus::CodeRequired => {
                    (Color32::YELLOW, "需要驗證碼", None)
                }
                TelegramStatus::PasswordRequired { hint } => {
                    (Color32::YELLOW, "需要 2FA 密碼", Some(hint.as_str()))
                }
                TelegramStatus::Error { message } => {
                    (Color32::from_rgb(220, 80, 80), "錯誤", Some(message.as_str()))
                }
            };
            ui.horizontal(|ui| {
                ui.colored_label(color, label);
                if let Some(d) = detail {
                    ui.small(d);
                }
            });
        });

        ui.separator();

        // Input forms based on current state
        match &status {
            TelegramStatus::CodeRequired => {
                ui.label("輸入 Telegram 驗證碼：");
                let submitted = ui
                    .horizontal(|ui| {
                        let r = ui.text_edit_singleline(&mut self.tg_code);
                        let btn = ui.button("提交");
                        (r.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            || btn.clicked()
                    })
                    .inner;
                if submitted && !self.tg_code.trim().is_empty() {
                    self.tg_auth.submit_code(self.tg_code.trim().to_string());
                    self.tg_code.clear();
                    self.tg_msg = "驗證碼已提交".to_string();
                }
            }
            TelegramStatus::PasswordRequired { hint } => {
                ui.label(format!("輸入 2FA 密碼（提示：{hint}）："));
                let submitted = ui
                    .horizontal(|ui| {
                        let r = ui
                            .add(TextEdit::singleline(&mut self.tg_password).password(true));
                        let btn = ui.button("提交");
                        (r.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            || btn.clicked()
                    })
                    .inner;
                if submitted && !self.tg_password.trim().is_empty() {
                    self.tg_auth.submit_password(self.tg_password.clone());
                    self.tg_password.clear();
                    self.tg_msg = "密碼已提交".to_string();
                }
            }
            _ => {
                ui.label("目前不需要輸入任何認證資訊。");
            }
        }

        if !self.tg_msg.is_empty() {
            ui.colored_label(Color32::from_rgb(100, 220, 100), &self.tg_msg);
        }
    }

    fn show_chat(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Agent Console").strong());
                ui.separator();
                let status_color = if self.chat_pending {
                    Color32::YELLOW
                } else {
                    Color32::from_rgb(100, 220, 100)
                };
                ui.colored_label(status_color, &self.agent_console.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("複製全部（Console + 對話 + Log）").clicked() {
                        let bundle = build_console_log_bundle(&self.agent_console, &self.chat_messages, 250);
                        ui.ctx().copy_text(bundle);
                        self.agent_console.status = "已複製 Console + 對話 + Log".to_string();
                    }
                    if ui.small_button("複製對話").clicked() {
                        ui.ctx().copy_text(chat_history_snapshot(&self.chat_messages));
                        self.agent_console.status = "已複製對話內容".to_string();
                    }
                    if ui.small_button("複製 Console + Log").clicked() {
                        let bundle = format!(
                            "=== Agent Console ===\n{}\n\n=== Recent Logs ===\n{}",
                            self.agent_console.snapshot_text(),
                            log_buffer::snapshot_text(200)
                        );
                        ui.ctx().copy_text(bundle);
                        self.agent_console.status = "已複製 Console + Log".to_string();
                    }
                    if ui.small_button("複製 Console").clicked() {
                        ui.ctx().copy_text(self.agent_console.snapshot_text());
                        self.agent_console.status = "已複製 Console".to_string();
                    }
                });
            });
            ui.small(format!("Route: {}", self.agent_console.route));
            if !self.agent_console.intent_family.is_empty() {
                ui.small(format!("Intent Family: {}", self.agent_console.intent_family));
            }
            if !self.agent_console.summary.is_empty() {
                ui.label(&self.agent_console.summary);
            }
            if !self.agent_console.steps.is_empty() {
                ui.small(format!("Steps: {}", self.agent_console.steps.join(" → ")));
            }
            if !self.agent_console.recommended_skills.is_empty() {
                ui.small(format!("Recommended Skills: {}", self.agent_console.recommended_skills.join(", ")));
            }
            if !self.agent_console.tools.is_empty() {
                ui.small(format!("Tools: {}", self.agent_console.tools.join(", ")));
            }
            if !self.agent_console.trace.is_empty() {
                ui.collapsing("Execution Trace", |ui| {
                    for item in &self.agent_console.trace {
                        ui.small(item);
                    }
                });
            }
            if !self.agent_console.latest_task_summary.is_empty() {
                ui.collapsing("Latest Research Summary", |ui| {
                    ui.small(&self.agent_console.latest_task_summary);
                });
            }
        });

        // ── Coding Console (shown when a coding task is active or recent) ─────
        if !self.coding_console.task.is_empty() {
            ui.add_space(4.0);
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(20, 30, 40))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("⚙ Coding Console").strong().color(Color32::from_rgb(100, 200, 255)));
                        ui.separator();
                        let status_color = if self.coding_console.status.contains("完成") || self.coding_console.status.contains("✅") {
                            Color32::from_rgb(100, 220, 100)
                        } else if self.coding_console.status.contains("錯誤") || self.coding_console.status.contains("Error") {
                            Color32::from_rgb(220, 80, 80)
                        } else {
                            Color32::YELLOW
                        };
                        ui.colored_label(status_color, &self.coding_console.status);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("複製 Coding Console").clicked() {
                                ui.ctx().copy_text(self.coding_console.snapshot_text());
                            }
                        });
                    });

                    if !self.coding_console.task.is_empty() {
                        ui.small(format!("Task: {}", self.coding_console.task.chars().take(80).collect::<String>()));
                    }
                    if self.coding_console.dry_run {
                        ui.colored_label(Color32::from_rgb(255, 200, 60), "⚠ Dry-run 模式：檔案未實際寫入");
                    }
                    if !self.coding_console.files_modified.is_empty() {
                        ui.small(format!("📁 Files: {}", self.coding_console.files_modified.join(", ")));
                    }
                    if self.coding_console.verified {
                        ui.colored_label(Color32::from_rgb(100, 220, 100), "✅ cargo check passed");
                    }
                    if !self.coding_console.trace.is_empty() {
                        ui.collapsing(format!("ReAct Trace ({} steps)", self.coding_console.trace.len()), |ui| {
                            ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
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
                        });
                    }
                    if let Some(ref diff) = self.coding_console.diff {
                        if !diff.trim().is_empty() {
                            ui.collapsing("📝 Git Diff", |ui| {
                                ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                                    let preview: String = diff.chars().take(2000).collect();
                                    ui.add(
                                        TextEdit::multiline(&mut preview.as_str())
                                            .code_editor()
                                            .desired_rows(8)
                                            .desired_width(f32::INFINITY),
                                    );
                                });
                            });
                        }
                    }
                    if let Some(ref vout) = self.coding_console.verification_output {
                        if !vout.trim().is_empty() {
                            ui.collapsing("🔍 Verification Output", |ui| {
                                ui.small(vout.chars().take(400).collect::<String>());
                            });
                        }
                    }
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
                                    task_clone, false, None,
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
                            rt.spawn(async move {
                                let resp = crate::agents::coding_agent::run_coding_via_adk(
                                    task_clone, true, None,
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

        ScrollArea::vertical()
            .max_height(available - input_area_height)
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                if self.chat_messages.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.colored_label(Color32::GRAY, "直接輸入訊息，與本地 AI 對話");
                        ui.small("使用 .env 設定的 LLM 後端（Ollama / LM Studio）");
                    });
                }

                for msg in &self.chat_messages {
                    let (bg, label, text_color) = match msg.role {
                        ChatRole::User => (
                            Color32::from_rgb(40, 60, 100),
                            "你",
                            Color32::WHITE,
                        ),
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
                    egui::Frame::new()
                        .fill(Color32::from_rgb(45, 55, 45))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .corner_radius(6.0)
                        .show(ui, |ui| {
                            ui.colored_label(Color32::GRAY, "Sirin");
                            ui.colored_label(Color32::YELLOW, "思考中…");
                        });
                }
            });

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
        let (submit, force_coding) = ui.horizontal(|ui| {
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

            // auto_approve_writes toggle — reads and immediately saves persona config.
            ui.separator();
            let mut auto_approve = crate::persona::Persona::load()
                .map(|p| p.coding_agent.auto_approve_writes)
                .unwrap_or(false);
            let toggle = ui
                .checkbox(&mut auto_approve, "自動允許寫入")
                .on_hover_text("關閉時，Coding Agent 寫入檔案前會彈出確認對話框（對應 persona.yaml 中的 auto_approve_writes）");
            if toggle.changed() {
                if let Ok(mut p) = crate::persona::Persona::load() {
                    p.coding_agent.auto_approve_writes = auto_approve;
                    let _ = p.save();
                }
            }

            // Plain Enter = send; Shift+Enter = newline.
            let enter_send = input.has_focus()
                && ui.input_mut(|i| {
                    if i.key_pressed(egui::Key::Enter) && !i.modifiers.shift {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                        true
                    } else {
                        false
                    }
                });

            (send.clicked() || enter_send, code_btn.clicked())
        })
        .inner;

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
            self.chat_messages.push(ChatMessage { role: ChatRole::User, text: task.clone() });
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
                let result = tokio::task::spawn(
                    crate::agents::coding_agent::run_coding_via_adk(task, false, None),
                )
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
                self.chat_messages.push(ChatMessage { role: ChatRole::User, text: user_text.clone() });
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
                        Err(e) => format!("❌ 技能執行失敗：{e}\n\n輸入 `/skill list` 查看可用技能。"),
                    }
                };
                self.chat_messages.push(ChatMessage { role: ChatRole::Assistant, text: reply });
            } else {
                // Check if this looks like a coding request that needs pre-flight confirmation.
                let is_coding_hint = crate::agents::router_agent::is_coding_request(&user_text);
                let needs_confirm = is_coding_hint
                    && crate::persona::Persona::load()
                        .map(|p| !p.coding_agent.auto_approve_writes)
                        .unwrap_or(false);

                if needs_confirm && self.pending_coding_confirmation.is_none() {
                    // Show confirmation dialog — don't submit yet.
                    self.pending_coding_confirmation = Some(user_text);
                    // Push a pending indicator message so the user knows we saw the input.
                    self.chat_messages.push(ChatMessage {
                        role: ChatRole::User,
                        text: self.pending_coding_confirmation.as_deref().unwrap_or("").to_string(),
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

                    let is_meta_request = crate::telegram::language::is_identity_question(&confirmed_user_text)
                        || crate::telegram::language::is_code_access_question(&confirmed_user_text);

                    let history: Vec<String> = self
                        .chat_messages
                        .windows(2)
                        .filter_map(|pair| {
                            if pair[0].role == ChatRole::User && pair[1].role == ChatRole::Assistant {
                                Some(format!("User: {}\nAssistant: {}", pair[0].text, pair[1].text))
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
                        self.agent_console.summary = "直接回答身份 / 看碼能力問題，不啟動 research。".to_string();
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
                                    });

                            let _ = coding_tx.try_send(CodingUiUpdate {
                                response: None,
                                status_msg: format!("🔄 執行中：{}", &coding_request.task),
                            });

                            let resp = crate::agents::coding_agent::run_coding_via_adk(
                                coding_request.task,
                                coding_request.dry_run,
                                None,
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
                                });
                            run_chat_and_send(request, plan_update, user_text_spawn, tx).await;
                        }
                    });
                }
            } // end else (not /skill command)
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

    let response = crate::agents::chat_agent::stream_chat_response(
        request,
        move |token| {
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
        },
    )
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
