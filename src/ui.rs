//! Native egui/eframe UI for Sirin.
//!
//! Runs on the main thread. Background Tokio tasks (Telegram listener,
//! follow-up worker) communicate via the same shared-state structs they
//! always have — no IPC layer needed.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, RichText, ScrollArea, TextEdit};
use tokio::runtime::Handle;

use crate::log_buffer;
use crate::memory::{append_context, ensure_codebase_index};
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

#[derive(Clone, Default)]
struct AgentConsoleState {
    route: String,
    summary: String,
    steps: Vec<String>,
    tools: Vec<String>,
    trace: Vec<String>,
    latest_task_summary: String,
    status: String,
}

#[derive(Clone)]
struct ChatUiUpdate {
    reply: String,
    tools: Vec<String>,
    trace: Vec<String>,
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
            tg_code: String::new(),
            tg_password: String::new(),
            tg_msg: String::new(),
            chat_messages: Vec::new(),
            chat_input: String::new(),
            chat_pending: false,
            agent_console: AgentConsoleState::default(),
            chat_tx,
            chat_rx,
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
            if let Ok(update) = self.chat_rx.try_recv() {
                self.chat_messages.push(ChatMessage {
                    role: ChatRole::Assistant,
                    text: update.reply,
                });
                self.chat_pending = false;
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
            } else {
                // Keep repainting while waiting so the spinner stays live.
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
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
                                // No-op: buffer keeps history, just a UX hint
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
            });
            ui.small(format!("Route: {}", self.agent_console.route));
            if !self.agent_console.summary.is_empty() {
                ui.label(&self.agent_console.summary);
            }
            if !self.agent_console.steps.is_empty() {
                ui.small(format!("Steps: {}", self.agent_console.steps.join(" → ")));
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
        ui.horizontal(|ui| {
            let input = ui.add_sized(
                [ui.available_width() - 70.0, 40.0],
                TextEdit::multiline(&mut self.chat_input)
                    .hint_text("輸入訊息…（Enter 送出，Shift+Enter 換行）")
                    .desired_rows(2),
            );

            let send = ui.add_enabled(
                !self.chat_pending && !self.chat_input.trim().is_empty(),
                egui::Button::new("送出").min_size([60.0, 40.0].into()),
            );

            // Intercept plain Enter (send); leave Shift+Enter as newline.
            let enter_send = input.has_focus()
                && ui.input_mut(|i| {
                    if i.key_pressed(egui::Key::Enter) && !i.modifiers.shift {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                        true
                    } else {
                        false
                    }
                });

            let submit = send.clicked() || enter_send;

            if submit && !self.chat_input.trim().is_empty() {
                let user_text = self.chat_input.trim().to_string();
                self.chat_messages.push(ChatMessage {
                    role: ChatRole::User,
                    text: user_text.clone(),
                });
                self.chat_input.clear();
                self.chat_pending = true;

                // Build context from recent chat history (last 5 turns).
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
                let context_block = if history.is_empty() {
                    None
                } else {
                    Some(history.join("\n---\n"))
                };

                let tx = self.chat_tx.clone();
                let rt = self.rt.clone();
                let planner_state = futures::executor::block_on(crate::agents::planner_agent::run_planner_via_adk(
                    crate::agents::planner_agent::PlannerRequest {
                        user_text: user_text.clone(),
                        context_block: context_block.clone(),
                        peer_id: Some(0),
                        fallback_reply: None,
                        execution_result: None,
                    },
                    None,
                ))
                .ok();

                if let Some(plan) = planner_state {
                    self.agent_console.route = match plan.intent {
                        crate::agents::planner_agent::PlanIntent::Research => "research".to_string(),
                        crate::agents::planner_agent::PlanIntent::Answer => "chat".to_string(),
                    };
                    self.agent_console.summary = plan.summary;
                    self.agent_console.steps = plan.steps;
                    self.agent_console.tools.clear();
                    self.agent_console.trace.clear();
                    self.agent_console.status = "Executing…".to_string();
                } else {
                    self.agent_console.route = "chat".to_string();
                    self.agent_console.summary = "Planner unavailable; using direct router fallback.".to_string();
                    self.agent_console.steps = vec!["route request".to_string(), "run chat response".to_string()];
                    self.agent_console.tools.clear();
                    self.agent_console.trace.clear();
                    self.agent_console.status = "Executing…".to_string();
                }

                rt.spawn(async move {
                    let routed = crate::agents::router_agent::run_router_via_adk(
                        crate::agents::router_agent::RouterRequest {
                            user_text: user_text.clone(),
                            context_block,
                            peer_id: Some(0),
                            fallback_reply: None,
                            execution_result: None,
                        },
                        None,
                    )
                    .await;

                    let response = match routed {
                        Ok(output) => {
                            let chat_request = output.get("chat_request").cloned().unwrap_or_default();
                            let request: crate::agents::chat_agent::ChatRequest = serde_json::from_value(chat_request)
                                .unwrap_or(crate::agents::chat_agent::ChatRequest {
                                    user_text: user_text.clone(),
                                    execution_result: None,
                                    context_block: None,
                                    fallback_reply: None,
                                    peer_id: Some(0),
                                });
                            crate::agents::chat_agent::run_chat_response_via_adk_with_tracker(request, None).await
                        }
                        Err(err) => crate::agents::chat_agent::ChatAgentResponse {
                            reply: format!("路由錯誤：{err}"),
                            ..Default::default()
                        },
                    };

                    let _ = append_context(&user_text, &response.reply, Some(0));
                    let _ = tx.send(ChatUiUpdate {
                        reply: response.reply,
                        tools: response.tools_used,
                        trace: response.trace,
                    });
                });
            }
        });
    }
}
