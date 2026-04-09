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

    // Activity log (all agents)
    tasks: Vec<TaskEntry>,

    // Research records
    research_tasks: Vec<ResearchTask>,
    research_topic: String,
    research_url: String,
    research_msg: String,
    research_msg_at: Option<std::time::Instant>,
    pending_objectives: Option<Vec<String>>,

    // Telegram OTP input (system panel)
    tg_code: String,
    tg_password: String,
    tg_msg: String,

    last_refresh: std::time::Instant,

    /// Subscriber for the process-wide agent event bus.  Drained every frame.
    event_rx: broadcast::Receiver<AgentEvent>,

    // ── UI state ──────────────────────────────────────────────────────────────
    /// Set of research task IDs whose full report is expanded.
    research_expanded: std::collections::HashSet<String>,

    // Settings
    settings_agents: Option<crate::agent_config::AgentsFile>,
    agent_auth_states: Vec<(String, crate::telegram_auth::TelegramAuthState)>,
    settings_msg: String,
    settings_msg_at: Option<std::time::Instant>,
    settings_agent_scratch: Vec<AgentUiScratch>,
    settings_new_agent_id: String,
    settings_new_agent_name: String,
    settings_active_tab: usize,

    // ── Agent workspace ───────────────────────────────────────────────────────
    /// Active sub-tab: 0=思考流, 1=待確認.
    workspace_tab: usize,
    /// Feedback message (approve/reject result).
    dispatch_msg: String,
    dispatch_msg_at: Option<std::time::Instant>,
    /// Pending replies cached for the currently selected agent.
    pending_replies: Vec<crate::pending_reply::PendingReply>,
    pending_replies_loaded_for: String,
    pending_draft_edits: std::collections::HashMap<String, String>,
    /// Pending reply counts cached per agent (refreshed every 5 s).
    pending_count_cache: std::collections::HashMap<String, usize>,

    // ── LLM 配置（本地模型選擇）─────────────────────────────────────────────
    llm_ui_config: crate::llm::LlmUiConfig,
    /// Models discovered by the last scan (Ollama/LM Studio query).
    llm_available_models: Vec<String>,
    /// Pending background scan result channel.
    llm_scan_rx: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    llm_config_msg: String,
    llm_config_msg_at: Option<std::time::Instant>,

    // ── Browser screenshot ────────────────────────────────────────────────────
    /// Last screenshot captured by the web_navigate tool (shown in 思考流 tab).
    browser_screenshot: Option<egui::TextureHandle>,
    /// URL of the last captured screenshot.
    browser_screenshot_url: String,

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
            research_msg_at: None,
            pending_objectives: None,
            tg_code: String::new(),
            tg_password: String::new(),
            tg_msg: String::new(),
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            event_rx: crate::events::subscribe(),
            research_expanded: std::collections::HashSet::new(),
            settings_agents: None,
            settings_msg: String::new(),
            settings_msg_at: None,
            settings_agent_scratch: Vec::new(),
            settings_new_agent_id: String::new(),
            settings_new_agent_name: String::new(),
            settings_active_tab: 0,
            agent_auth_states,
            workspace_tab: 0,
            dispatch_msg: String::new(),
            dispatch_msg_at: None,
            pending_replies: Vec::new(),
            pending_replies_loaded_for: String::new(),
            pending_draft_edits: std::collections::HashMap::new(),
            pending_count_cache: std::collections::HashMap::new(),
            llm_ui_config: {
                let cfg = crate::llm::LlmUiConfig::load();
                if cfg.provider.is_empty() && cfg.main_model.is_empty() {
                    // Bootstrap from current active singleton
                    let llm = crate::llm::shared_llm();
                    crate::llm::LlmUiConfig {
                        provider:      llm.backend_name().to_string(),
                        base_url:      llm.base_url.clone(),
                        main_model:    llm.model.clone(),
                        router_model:  llm.router_model.clone().unwrap_or_default(),
                        coding_model:  llm.coding_model.clone().unwrap_or_default(),
                        large_model:   llm.large_model.clone().unwrap_or_default(),
                    }
                } else {
                    cfg
                }
            },
            llm_available_models: Vec::new(),
            llm_scan_rx: None,
            llm_config_msg: String::new(),
            llm_config_msg_at: None,
            browser_screenshot: None,
            browser_screenshot_url: String::new(),
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
        // Pick up any pending persona objective proposal from the researcher.
        if let Some(proposed) = researcher::take_pending_objectives() {
            self.pending_objectives = Some(proposed);
        }
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
        crate::skill_loader::invalidate_cache();
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

        // ── Poll background model scan result ────────────────────────────────
        if let Some(ref rx) = self.llm_scan_rx {
            if let Ok(models) = rx.try_recv() {
                self.llm_available_models = models;
                self.llm_scan_rx = None;
                ctx.request_repaint();
            }
        }

        // ── Timed auto-dismiss: LLM config message (4 s) ─────────────────────
        if let Some(at) = self.llm_config_msg_at {
            if at.elapsed() > std::time::Duration::from_secs(4) {
                self.llm_config_msg.clear();
                self.llm_config_msg_at = None;
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

        // Auto-refresh every 5 s.
        if self.last_refresh.elapsed() > std::time::Duration::from_secs(5) {
            self.refresh();
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(5));

        // Drain agent-event-bus messages (non-blocking).
        loop {
            match self.event_rx.try_recv() {
                Ok(AgentEvent::ResearchRequested { topic, url }) => {
                    // Switch to workspace → 思考流 sub-tab and kick off the research run.
                    self.view = View::Agent(self.view_agent_idx());
                    self.workspace_tab = 0;
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
                Ok(AgentEvent::CodingAgentCompleted { .. }) => {
                    // Refresh task list immediately so the result shows without waiting 5 s.
                    self.refresh();
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
                    self.workspace_tab = 1;
                }
                Ok(AgentEvent::BrowserScreenshotReady { png_bytes, url }) => {
                    // Decode PNG and upload as egui texture for display.
                    if let Ok(img) = image::load_from_memory(&png_bytes) {
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &rgba,
                        );
                        self.browser_screenshot = Some(ctx.load_texture(
                            "browser_screenshot",
                            color_image,
                            egui::TextureOptions::LINEAR,
                        ));
                        self.browser_screenshot_url = url;
                    }
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
                                    "⚙ 系統",
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

        // Keep workspace selection in sync.
        if self.pending_replies_loaded_for != agent_id {
            self.pending_replies = crate::pending_reply::load_pending(&agent_id);
            self.pending_replies_loaded_for = agent_id.clone();
        }
        let pending_count = self.pending_replies.iter()
            .filter(|r| r.status == PendingStatus::Pending)
            .count();

        // ── Agent header ──────────────────────────────────────────────────────
        let toggle_clicked = ui.horizontal(|ui| {
            let led = if agent.enabled { Color32::from_rgb(80, 200, 100) } else { Color32::GRAY };
            ui.colored_label(led, "●");
            ui.label(RichText::new(&agent_name).heading().strong());
            ui.colored_label(Color32::GRAY, format!("  {}", agent_id));
            let mut clicked = false;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let tg_status = self.agent_auth_states.iter()
                    .find(|(id, _)| id == &agent_id)
                    .map(|(_, s)| s.status());
                if let Some(ref st) = tg_status { tg_status_badge(ui, st); }
                let (label, color) = if agent.enabled {
                    ("ON", Color32::from_rgb(80, 200, 100))
                } else {
                    ("OFF", Color32::GRAY)
                };
                if ui.add(egui::Button::new(RichText::new(label).color(color)).frame(true)).clicked() {
                    clicked = true;
                }
                let platform_label = match agent.platform() {
                    crate::agent_config::AgentPlatform::Telegram => "✈ Telegram",
                    crate::agent_config::AgentPlatform::Teams    => "💼 Teams",
                    crate::agent_config::AgentPlatform::UiOnly   => "🖥 UI",
                };
                ui.colored_label(Color32::DARK_GRAY, RichText::new(platform_label).small());
            });
            clicked
        }).inner;
        if toggle_clicked {
            if let Some(f) = self.settings_agents.as_mut() {
                if let Some(a) = f.agents.get_mut(sel) {
                    a.enabled = !a.enabled;
                }
            }
        }
        if !self.dispatch_msg.is_empty() {
            let color = if self.dispatch_msg.starts_with('❌') {
                Color32::from_rgb(220, 80, 80)
            } else {
                Color32::from_rgb(100, 220, 100)
            };
            ui.colored_label(color, &self.dispatch_msg);
        }
        ui.separator();

        // ── Sub-tab bar (3 tabs) ──────────────────────────────────────────────
        ui.horizontal(|ui| {
            let tabs: &[(&str, Option<usize>)] = &[
                ("思考流", None),
                ("待確認", if pending_count > 0 { Some(pending_count) } else { None }),
                ("⚙ 設定",  None),
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
            // ── 思考流 ────────────────────────────────────────────────────────
            0 => {
                if self.tasks.is_empty() {
                    ui.colored_label(Color32::GRAY, "尚無任務記錄。");
                } else {
                    ScrollArea::vertical().id_salt("ws_tasks").auto_shrink(false).show(ui, |ui| {
                        for task in &self.tasks {
                            let is_summary = task.event == "research_summary_ready";
                            let status = task.status.as_deref().unwrap_or("");
                            let (badge, fg, bg) = task_status_badge(status, task.reason.as_deref(), is_summary);
                            egui::Frame::new()
                                .fill(bg)
                                .corner_radius(4.0)
                                .inner_margin(egui::Margin::symmetric(6, 3))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.colored_label(fg, badge);
                                        ui.colored_label(Color32::GRAY, RichText::new(&task.event).small());
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.colored_label(Color32::DARK_GRAY, RichText::new(
                                                task.timestamp.get(11..16).unwrap_or("—")
                                            ).small());
                                        });
                                    });
                                    if let Some(r) = &task.reason {
                                        let snippet: String = r.chars().take(160).collect();
                                        ui.colored_label(Color32::GRAY, RichText::new(snippet).small());
                                    }
                                });
                            ui.add_space(2.0);
                        }
                    });
                }
                // ── Browser screenshot (shown when web_navigate captures a page) ──
                if let Some(tex) = &self.browser_screenshot {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("🌐 截圖").strong().small());
                        ui.colored_label(Color32::GRAY, RichText::new(&self.browser_screenshot_url).small());
                    });
                    let max_w = ui.available_width();
                    let size = tex.size_vec2();
                    let scale = (max_w / size.x).min(1.0);
                    ui.image((tex.id(), size * scale));
                }
            }

            // ── 待確認 ────────────────────────────────────────────────────────
            1 => {
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
                    // P1：Teams 草稿核准後立即通知 run_poller() 發送
                    if agent_id == "teams" {
                        crate::teams::notify_approved(id.clone());
                        self.dispatch_msg = "✅ 已批准，正在發送至 Teams…".to_string();
                    } else {
                        self.dispatch_msg = "✅ 已批准".to_string();
                    }
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

            // ── ⚙ 設定 ────────────────────────────────────────────────────────
            _ => self.show_agent_settings_tab(ui, sel),
        }
    }

    fn show_agent_settings_tab(&mut self, ui: &mut egui::Ui, sel: usize) {
        use crate::agent_config::AgentsFile;

        if self.settings_agents.is_none() {
            self.settings_agents = AgentsFile::load().ok().or_else(|| Some(AgentsFile::default()));
        }

        let mut do_save   = false;
        let mut do_reload = false;
        let settings_msg  = self.settings_msg.clone();
        let mut active_tab = self.settings_active_tab;

        // Take scratch out to avoid simultaneous &mut self borrows.
        let mut scratch = std::mem::take(&mut self.settings_agent_scratch);
        let agent_auth_states: Vec<_> = self.agent_auth_states.iter()
            .map(|(id, s)| (id.clone(), s.clone())).collect();
        let all_tg_phones: Vec<(String, String)> = self.settings_agents.as_ref()
            .map(|f| f.agents.iter()
                .filter_map(|a| {
                    let phone = a.channel.as_ref()?.telegram.as_ref().map(|t| t.phone.clone())?;
                    Some((a.id.clone(), phone))
                })
                .collect())
            .unwrap_or_default();

        {
            let agents_file = self.settings_agents.as_mut().unwrap();
            scratch.resize_with(agents_file.agents.len(), Default::default);

            if sel < agents_file.agents.len() {
                // Toolbar
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.add(egui::Button::new(RichText::new("💾 儲存").strong())
                                .fill(Color32::from_rgb(30, 90, 50))).clicked() { do_save = true; }
                            if ui.button("↺ 重新載入").clicked() { do_reload = true; }
                            if !settings_msg.is_empty() {
                                let color = if settings_msg.starts_with('❌') {
                                    Color32::from_rgb(220, 80, 80)
                                } else { Color32::from_rgb(100, 220, 100) };
                                ui.colored_label(color, &settings_msg);
                            }
                        });
                    });
                ui.separator();

                let agent = &mut agents_file.agents[sel];
                let scratch_entry = &mut scratch[sel];
                let auth_state = agent_auth_states.iter()
                    .find(|(id, _)| id == &agent.id)
                    .map(|(_, s)| s);
                let other_tg_phones: Vec<_> = all_tg_phones.iter()
                    .filter(|(id, _)| id != &agent.id)
                    .cloned()
                    .collect();
                ScrollArea::vertical().id_salt("ws_agent_cfg").auto_shrink(false).show(ui, |ui| {
                    show_agent_detail(ui, agent, scratch_entry, auth_state, &other_tg_phones, &mut active_tab);
                });
            }
        }

        self.settings_agent_scratch = scratch;
        self.settings_active_tab    = active_tab;

        if do_save {
            match self.settings_agents.as_ref().unwrap().save() {
                Ok(()) => { self.settings_msg = "✅ 已儲存".to_string(); self.refresh(); }
                Err(e) => { self.settings_msg = format!("❌ 儲存失敗：{e}"); }
            }
            self.settings_msg_at = Some(std::time::Instant::now());
        }
        if do_reload {
            match AgentsFile::load() {
                Ok(fresh) => { self.settings_agents = Some(fresh); self.settings_agent_scratch.clear(); self.settings_msg = "已重新載入".to_string(); }
                Err(e)    => { self.settings_msg = format!("❌ 載入失敗：{e}"); }
            }
            self.settings_msg_at = Some(std::time::Instant::now());
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        use crate::agent_config::AgentsFile;

        if self.settings_agents.is_none() {
            self.settings_agents = AgentsFile::load().ok().or_else(|| Some(AgentsFile::default()));
        }

        // ── Add agent row ─────────────────────────────────────────────────────
        let mut new_id   = std::mem::take(&mut self.settings_new_agent_id);
        let mut new_name = std::mem::take(&mut self.settings_new_agent_name);
        let mut do_add   = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("新增 Agent").strong().small());
            ui.add(egui::TextEdit::singleline(&mut new_id).hint_text("id").desired_width(70.0));
            ui.add(egui::TextEdit::singleline(&mut new_name).hint_text("名稱").desired_width(90.0));
            let can_add = !new_id.trim().is_empty() && !new_name.trim().is_empty();
            if ui.add_enabled(can_add, egui::Button::new("＋")).clicked() { do_add = true; }
        });
        if do_add {
            let id   = new_id.trim().to_string();
            let name = if new_name.trim().is_empty() { id.clone() } else { new_name.trim().to_string() };
            if let Some(f) = self.settings_agents.as_mut() {
                let new_idx = f.agents.len();
                f.agents.push(crate::agent_config::AgentConfig::new_default(&id, name));
                // Switch to new agent's workspace
                self.view = View::Agent(Some(new_idx));
                self.workspace_tab = 2; // ⚙ 設定 tab
            }
            new_id.clear();
            new_name.clear();
        }
        self.settings_new_agent_id   = new_id;
        self.settings_new_agent_name = new_name;

        ui.separator();

        // ── 本地模型配置 ──────────────────────────────────────────────────────
        {
            let cfg = &mut self.llm_ui_config;

            ui.label(RichText::new("本地模型配置").strong());
            ui.add_space(4.0);

            // Provider selector
            ui.horizontal(|ui| {
                ui.label(RichText::new("後端").small());
                for (label, val) in [("Ollama", "ollama"), ("LM Studio", "lmstudio"), ("Gemini", "gemini")] {
                    let active = cfg.provider == val;
                    let btn = egui::Button::new(RichText::new(label).small())
                        .fill(if active { Color32::from_rgb(35, 65, 110) } else { Color32::TRANSPARENT });
                    if ui.add(btn).clicked() { cfg.provider = val.to_string(); }
                }
            });

            // Base URL
            ui.horizontal(|ui| {
                ui.label(RichText::new("Base URL").small());
                ui.add(egui::TextEdit::singleline(&mut cfg.base_url)
                    .hint_text(if cfg.provider == "lmstudio" { "http://localhost:1234/v1" } else { "http://localhost:11434" })
                    .desired_width(220.0));

                let scanning = self.llm_scan_rx.is_some();
                let btn = egui::Button::new(RichText::new(if scanning { "⏳" } else { "🔍 掃描" }).small());
                if ui.add_enabled(!scanning, btn).on_hover_text("查詢可用模型").clicked() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.llm_scan_rx = Some(rx);
                    let base_url = cfg.base_url.clone();
                    let provider = cfg.provider.clone();
                    self.rt.spawn(async move {
                        let models = crate::llm::list_local_models(&base_url, &provider).await;
                        let _ = tx.send(models);
                    });
                }
            });

            // Available models list (populated after scan)
            if !self.llm_available_models.is_empty() {
                let names = self.llm_available_models.clone();
                ui.add_space(4.0);
                ui.colored_label(Color32::GRAY, RichText::new(
                    format!("掃描到 {} 個模型", names.len())
                ).small());

                // Helper: draw a model combobox
                fn model_combo<'a>(
                    ui: &mut egui::Ui,
                    id: &str,
                    label: &str,
                    current: &mut String,
                    options: &[String],
                    fallback_hint: &str,
                ) {
                    ui.label(RichText::new(label).small());
                    egui::ComboBox::from_id_salt(id)
                        .selected_text(if current.is_empty() { fallback_hint } else { current.as_str() })
                        .width(160.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(current, String::new(), format!("（{fallback_hint}）"));
                            for m in options {
                                ui.selectable_value(current, m.clone(), m.as_str());
                            }
                        });
                }

                let cfg = &mut self.llm_ui_config;
                egui::Grid::new("llm_model_grid").num_columns(4).spacing([8.0, 4.0]).show(ui, |ui| {
                    model_combo(ui, "llm_main",   "主模型",   &mut cfg.main_model,   &names, "必填");
                    model_combo(ui, "llm_router", "路由",     &mut cfg.router_model, &names, "同主模型");
                    ui.end_row();
                    model_combo(ui, "llm_coding", "Coding",  &mut cfg.coding_model, &names, "同主模型");
                    model_combo(ui, "llm_large",  "大模型",   &mut cfg.large_model,  &names, "同主模型");
                    ui.end_row();
                });
            } else if self.llm_scan_rx.is_some() {
                ui.colored_label(Color32::GRAY, RichText::new("掃描中…").small());
            }

            // Save button
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("💾 儲存模型配置").small())
                    .fill(Color32::from_rgb(30, 90, 50))).clicked()
                {
                    match self.llm_ui_config.save() {
                        Ok(()) => { self.llm_config_msg = "✅ 已儲存 config/llm.yaml，重啟後生效".to_string(); }
                        Err(e) => { self.llm_config_msg = format!("❌ 儲存失敗：{e}"); }
                    }
                    self.llm_config_msg_at = Some(std::time::Instant::now());
                }
                if !self.llm_config_msg.is_empty() {
                    let color = if self.llm_config_msg.starts_with('❌') {
                        Color32::from_rgb(220, 80, 80)
                    } else { Color32::from_rgb(100, 220, 100) };
                    ui.colored_label(color, RichText::new(&self.llm_config_msg).small());
                }
            });
        }

        ui.separator();

        // ── System panel (LLM summary + Telegram auth) ───────────────────────
        let mut tg_code     = std::mem::take(&mut self.tg_code);
        let mut tg_password = std::mem::take(&mut self.tg_password);
        let tg_msg          = self.tg_msg.clone();
        let tg_auth         = self.tg_auth.clone();
        let mut tg_msg_update: Option<String> = None;

        show_system_panel(ui, &self.rt, &tg_auth, &mut tg_code, &mut tg_password, &tg_msg, &mut tg_msg_update);

        self.tg_code     = tg_code;
        self.tg_password = tg_password;
        if let Some(msg) = tg_msg_update { self.tg_msg = msg; }
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
    // ── Tab bar ───────────────────────────────────────────────────────────
    let tabs = ["身分", "目標", "通訊"];
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
        1 => show_tab_goals(ui, agent, scratch),
        _ => show_tab_channel(ui, agent, auth, other_tg_phones),
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
        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new("回覆確認").strong().small());
        let tg_require = agent.channel.as_mut()
            .and_then(|c| c.telegram.as_mut())
            .map(|t| &mut t.require_confirmation);
        if let Some(require_conf) = tg_require {
            ui.checkbox(require_conf, "需要人工確認（AI 草稿不直接發送，等待確認）");
        }
    } else {
        ui.add_space(8.0);
        ui.colored_label(Color32::GRAY, "（無 Channel — UI / 測試模式）");
    }

    // ── 仿人類行為 ────────────────────────────────────────────────────────
    ui.add_space(10.0);
    ui.separator();
    let hb = &mut agent.human_behavior;
    ui.horizontal(|ui| {
        ui.checkbox(&mut hb.enabled, RichText::new("仿人類行為").strong().small());
        if hb.enabled {
            ui.colored_label(Color32::from_rgb(255, 200, 60), RichText::new("（延遲回覆中）").small());
        }
    });
    if hb.enabled {
        ui.add_space(4.0);
        egui::Grid::new("hb_mini").num_columns(4).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("延遲").small());
            let mut min_s = hb.min_reply_delay_secs as i64;
            if ui.add(egui::DragValue::new(&mut min_s).range(0..=3600).suffix("s")).changed() {
                hb.min_reply_delay_secs = min_s.max(0) as u64;
                if hb.min_reply_delay_secs > hb.max_reply_delay_secs {
                    hb.max_reply_delay_secs = hb.min_reply_delay_secs;
                }
            }
            ui.label(RichText::new("—").small());
            let mut max_s = hb.max_reply_delay_secs as i64;
            if ui.add(egui::DragValue::new(&mut max_s).range(0..=3600).suffix("s")).changed() {
                hb.max_reply_delay_secs = max_s.max(0) as u64;
                if hb.max_reply_delay_secs < hb.min_reply_delay_secs {
                    hb.min_reply_delay_secs = hb.max_reply_delay_secs;
                }
            }
            ui.end_row();
        });
    }
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


fn show_system_panel(
    ui: &mut egui::Ui,
    rt: &tokio::runtime::Handle,
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

    // ── Active LLM summary (read-only) ───────────────────────────────────
    ui.label(RichText::new("目前使用中").strong().small());
    ui.add_space(2.0);
    let llm = crate::llm::shared_llm();
    let (c, icon) = if llm.is_remote() { (Color32::from_rgb(255,160,60), "☁") } else { (Color32::from_rgb(100,220,100), "🖥") };
    ui.horizontal(|ui| {
        ui.colored_label(c, icon);
        ui.colored_label(Color32::GRAY, RichText::new(format!("{} / {}", llm.backend_name(), llm.model)).small().monospace());
    });
    ui.colored_label(Color32::DARK_GRAY, RichText::new("（啟動時載入，重啟後生效）").small());

    ui.add_space(8.0);
    // ── RPC server status ─────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.label(RichText::new("⚡  RPC").strong().small());
        if crate::rpc_server::is_running() {
            ui.colored_label(Color32::from_rgb(100, 220, 100),
                RichText::new(format!("{} ● 監聽中", crate::rpc_server::RPC_ADDR)).small().monospace());
        } else {
            ui.colored_label(Color32::GRAY, RichText::new("○ 未啟動").small());
        }
    });

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(6.0);

    // ── Teams 整合 ────────────────────────────────────────────────────────
    ui.label(RichText::new("💼  Microsoft Teams").strong());
    ui.add_space(4.0);

    use crate::teams::SessionStatus;
    let teams_status = crate::teams::session_status();

    // 狀態列
    ui.horizontal(|ui| {
        let (col, txt) = match &teams_status {
            SessionStatus::NotStarted      => (Color32::GRAY,                    "○ 未連線"),
            SessionStatus::WaitingForLogin => (Color32::YELLOW,                  "⏳ 等待登入…"),
            SessionStatus::Running         => (Color32::from_rgb(100, 220, 100), "● 監聽中"),
            SessionStatus::Error(_)        => (Color32::from_rgb(220, 80, 80),   "✗ 錯誤"),
        };
        ui.colored_label(col, txt);
        if let SessionStatus::Error(msg) = &teams_status {
            ui.colored_label(Color32::GRAY, RichText::new(format!("  {msg}")).small());
        }
    });

    ui.add_space(6.0);

    match &teams_status {
        SessionStatus::NotStarted | SessionStatus::Error(_) => {
            // ── 流程說明 ─────────────────────────────────────────────────────
            egui::Frame::none()
                .fill(Color32::from_rgb(28, 32, 38))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .rounding(4.0)
                .show(ui, |ui| {
                    ui.label(RichText::new("連線流程").small().strong());
                    ui.add_space(3.0);
                    for (n, line) in [
                        ("1", "點「開始連線」→ Chrome 視窗跳出"),
                        ("2", "若已有登入記錄（data/teams_profile）→ 自動進入 Teams"),
                        ("3", "若第一次或 session 過期 → 手動完成學校 SSO / MFA"),
                        ("4", "登入後狀態變為「● 監聽中」，無需再次操作"),
                        ("5", "收到訊息時 Sirin 自動回「稍等」並在此建立草稿"),
                        ("6", "草稿確認後點「✅ 確認發送」立即送出至 Teams"),
                    ] {
                        ui.horizontal(|ui| {
                            ui.colored_label(Color32::from_rgb(100,160,220),
                                RichText::new(n).small().monospace());
                            ui.colored_label(Color32::GRAY, RichText::new(line).small());
                        });
                    }
                    ui.add_space(3.0);
                    ui.colored_label(Color32::DARK_GRAY,
                        RichText::new("登入狀態儲存於 data/teams_profile（重啟後免重新登入）").small());
                });

            ui.add_space(8.0);
            let btn = egui::Button::new(
                RichText::new("  開始連線 Teams  ").small()
            ).fill(Color32::from_rgb(0, 80, 160));
            if ui.add(btn).clicked() {
                rt.spawn(crate::teams::run_poller());
            }
        }

        SessionStatus::WaitingForLogin => {
            egui::Frame::none()
                .fill(Color32::from_rgb(40, 36, 10))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .rounding(4.0)
                .show(ui, |ui| {
                    ui.colored_label(Color32::YELLOW,
                        RichText::new("請在跳出的 Chrome 視窗完成 Microsoft 登入").small());
                    ui.add_space(2.0);
                    ui.colored_label(Color32::GRAY,
                        RichText::new("• 學校帳號請選「Use another account」或直接輸入學校 email").small());
                    ui.colored_label(Color32::GRAY,
                        RichText::new("• 完成 MFA / SSO 後此狀態將自動更新（最長等待 5 分鐘）").small());
                    ui.colored_label(Color32::GRAY,
                        RichText::new("• 首次登入後 cookie 將永久保存，下次免登入").small());
                });
        }

        SessionStatus::Running => {
            egui::Frame::none()
                .fill(Color32::from_rgb(10, 36, 14))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .rounding(4.0)
                .show(ui, |ui| {
                    ui.colored_label(Color32::from_rgb(100, 220, 100),
                        RichText::new("Teams 已連線，正在監聽新訊息").small());
                    ui.add_space(2.0);
                    ui.colored_label(Color32::GRAY,
                        RichText::new("• 偵測延遲 < 100ms（CDP MutationObserver 事件驅動）").small());
                    ui.colored_label(Color32::GRAY,
                        RichText::new("• 收到訊息 → 自動回「稍等」→ 在「待確認」tab 建草稿").small());
                    ui.colored_label(Color32::GRAY,
                        RichText::new("• 點「✅ 確認發送」後立即送出，無需等待").small());
                });
        }
    }

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
