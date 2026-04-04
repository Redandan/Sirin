//! Native egui/eframe UI for Sirin.
//!
//! Runs on the main thread. Background Tokio tasks (Telegram listener,
//! follow-up worker) communicate via the same shared-state structs they
//! always have — no IPC layer needed.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, RichText, ScrollArea, TextEdit};
use tokio::runtime::Handle;

use crate::llm::LlmConfig;
use crate::log_buffer;
use crate::memory::append_context;
use crate::persona::{Persona, TaskEntry, TaskTracker};
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
    chat_tx: std::sync::mpsc::SyncSender<String>,
    chat_rx: std::sync::mpsc::Receiver<String>,

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
            if let Ok(reply) = self.chat_rx.try_recv() {
                self.chat_messages.push(ChatMessage { role: ChatRole::Assistant, text: reply });
                self.chat_pending = false;
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
                let status_color = match status {
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
                        .unwrap_or(&task.event);
                    let label_text = format!("{} — {}", task.event,
                        preview.chars().take(80).collect::<String>());
                    ui.add_sized(
                        [col_widths[1], row_height],
                        egui::Label::new(&label_text).truncate(),
                    );
                    ui.colored_label(status_color, status);
                });
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
                        let task = researcher::run_research(topic, url).await;
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
        // ── Message history ───────────────────────────────────────────────────
        let available = ui.available_height();
        let input_area_height = 60.0;

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

                    egui::Frame::none()
                        .fill(bg)
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .rounding(6.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.colored_label(Color32::GRAY, label);
                            });
                            ui.colored_label(text_color, &msg.text);
                        });
                    ui.add_space(4.0);
                }

                if self.chat_pending {
                    egui::Frame::none()
                        .fill(Color32::from_rgb(45, 55, 45))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .rounding(6.0)
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
                rt.spawn(async move {
                    let llm = LlmConfig::from_env();
                    let client = reqwest::Client::new();
                    let persona = Persona::load().ok();
                    let persona_name = persona.as_ref().map(|p| p.name()).unwrap_or("Sirin");
                    let voice = persona
                        .as_ref()
                        .map(|p| p.response_style.voice.as_str())
                        .unwrap_or("自然、友善、簡潔");

                    let history_block = context_block
                        .map(|h| format!("\nRecent conversation:\n{h}"))
                        .unwrap_or_default();

                    let prompt = format!(
                        "You are {persona_name}.\nPersonality: {voice}\n\
Task: Reply to the user's message naturally and helpfully.\n\
Rules:\n\
- Reply in the same language as the user.\n\
- Keep it concise (1-4 sentences).\n\
- No system-prompt style phrasing.\n\
{history_block}\n\
User: {user_text}\n\
Reply:"
                    );

                    let reply = match crate::llm::call_prompt(&client, &llm, prompt).await {
                        Ok(r) if !r.trim().is_empty() => r.trim().to_string(),
                        Ok(_) => "(空回覆)".to_string(),
                        Err(e) => format!("LLM 錯誤：{e}"),
                    };

                    // Save to per-peer context (GUI chat uses peer_id = 0).
                    let _ = append_context(&user_text, &reply, Some(0));

                    let _ = tx.send(reply);
                });
            }
        });
    }
}
