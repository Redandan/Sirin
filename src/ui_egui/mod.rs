//! egui immediate-mode UI for Sirin.
//! Catppuccin Mocha theme + AppService trait.

mod theme;
mod sidebar;
mod workspace;
mod settings;
mod log_view;
mod workflow;
mod meeting;

use std::sync::Arc;
use std::collections::VecDeque;

use eframe::egui::{self, RichText};

use crate::ui_service::*;

// ── View ─────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone)]
enum View { Workspace(usize), Settings, Log, Workflow, Meeting }

// ── Toast ────────────────────────────────────────────────────────────────────

struct Toast {
    text: String,
    level: ToastLevel,
    expires: std::time::Instant,
}

impl Toast {
    fn from_event(e: ToastEvent) -> Self {
        Self { text: e.text, level: e.level, expires: std::time::Instant::now() + std::time::Duration::from_secs(4) }
    }
}

// ── App ──────────────────────────────────────────────────────────────────────

pub struct SirinApp {
    svc: Arc<dyn AppService>,
    view: View,
    agents: Vec<AgentSummary>,
    tasks: Vec<TaskView>,
    pending_counts: std::collections::HashMap<String, usize>,
    last_refresh: std::time::Instant,
    toasts: VecDeque<Toast>,
    // Sidebar rename state (replaces unsafe static mut)
    renaming: Option<(usize, String)>,

    log_state: log_view::LogState,
    workspace_state: workspace::WorkspaceState,
    settings_state: settings::SettingsState,
    workflow_state: workflow::WorkflowUiState,
    meeting_state: meeting::MeetingState,
}

impl SirinApp {
    pub fn new(svc: Arc<dyn AppService>, cc: &eframe::CreationContext) -> Self {
        setup_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);
        let agents = svc.list_agents();
        Self {
            svc, view: View::Workspace(0), agents,
            tasks: Vec::new(),
            pending_counts: std::collections::HashMap::new(),
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            toasts: VecDeque::new(),
            renaming: None,
            log_state: Default::default(), workspace_state: Default::default(),
            settings_state: Default::default(), workflow_state: Default::default(),
            meeting_state: Default::default(),
        }
    }

    fn refresh(&mut self) {
        self.agents = self.svc.list_agents();
        self.tasks = self.svc.recent_tasks(200);
        self.pending_counts.clear();
        for a in &self.agents { self.pending_counts.insert(a.id.clone(), self.svc.pending_count(&a.id)); }
        self.last_refresh = std::time::Instant::now();
    }
}

impl eframe::App for SirinApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_refresh.elapsed() > std::time::Duration::from_secs(5) { self.refresh(); }
        ctx.request_repaint_after(std::time::Duration::from_secs(5));

        for te in self.svc.poll_toasts() { self.toasts.push_back(Toast::from_event(te)); }
        let now = std::time::Instant::now();
        self.toasts.retain(|t| t.expires > now);

        sidebar::show(ctx, &self.svc, &self.agents, &self.pending_counts, &mut self.view, &mut self.renaming);

        // ── Top bar ───────────────────────────────────────────────────────
        egui::TopBottomPanel::top("top_bar")
            .frame(egui::Frame::new().fill(theme::MANTLE).inner_margin(egui::vec2(16.0, 8.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Page title + breadcrumb
                    let (icon, title, subtitle) = match &self.view {
                        View::Workspace(idx) => {
                            let name = self.agents.get(*idx).map(|a| a.name.as_str()).unwrap_or("—");
                            ("💬", name, "Agent 工作區")
                        }
                        View::Settings => ("⚙", "系統設定", "LLM / TG / MCP / 技能"),
                        View::Log => ("📋", "系統 Log", "即時日誌"),
                        View::Workflow => ("🔧", "Skill 開發", "工作流 Pipeline"),
                        View::Meeting => ("🤝", "會議室", "多 Agent 協作"),
                    };
                    ui.label(RichText::new(icon).size(18.0));
                    ui.label(RichText::new(title).strong().size(16.0).color(theme::TEXT));
                    ui.colored_label(theme::OVERLAY0, RichText::new(format!("/ {subtitle}")).small());

                    // Right side: agent count + pending total
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let total_pending: usize = self.pending_counts.values().sum();
                        if total_pending > 0 {
                            theme::count_badge(ui, total_pending);
                            ui.colored_label(theme::OVERLAY0, RichText::new("待審").small());
                        }
                        ui.colored_label(theme::SURFACE2, RichText::new("|").small());
                        ui.colored_label(theme::OVERLAY0, RichText::new(format!("{} agents", self.agents.len())).small());
                    });
                });
            });

        // ── Central panel ────────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BASE).inner_margin(theme::GAP_LG))
            .show(ctx, |ui| {
                match self.view.clone() {
                    View::Workspace(idx) => workspace::show(ui, &self.svc, &self.agents, idx, &self.tasks, &self.pending_counts, &mut self.workspace_state),
                    View::Settings => settings::show(ui, &self.svc, &self.agents, &mut self.settings_state),
                    View::Log => log_view::show(ui, &self.svc, &mut self.log_state),
                    View::Workflow => workflow::show(ui, &self.svc, &mut self.workflow_state),
                    View::Meeting => meeting::show(ui, &self.svc, &self.agents, &mut self.meeting_state),
                }
            });

        // Toast overlay
        if !self.toasts.is_empty() {
            egui::Area::new(egui::Id::new("toasts"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
                .show(ctx, |ui| {
                    for toast in self.toasts.iter().rev().take(3) {
                        let (fg, bg) = match toast.level {
                            ToastLevel::Success => (theme::GREEN, theme::GREEN.linear_multiply(0.12)),
                            ToastLevel::Error => (theme::RED, theme::RED.linear_multiply(0.12)),
                            ToastLevel::Info => (theme::BLUE, theme::BLUE.linear_multiply(0.12)),
                        };
                        egui::Frame::new().fill(bg).corner_radius(8.0)
                            .inner_margin(egui::vec2(14.0, 8.0))
                            .stroke(egui::Stroke::new(1.0, fg.linear_multiply(0.3)))
                            .show(ui, |ui| { ui.colored_label(fg, &toast.text); });
                        ui.add_space(4.0);
                    }
                });
        }
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let paths = if cfg!(target_os = "windows") { vec!["C:\\Windows\\Fonts\\msjh.ttc", "C:\\Windows\\Fonts\\msyh.ttc"] }
    else if cfg!(target_os = "macos") { vec!["/System/Library/Fonts/PingFang.ttc"] }
    else { vec!["/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"] };
    for path in paths {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "cjk".into());
            fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".into());
            ctx.set_fonts(fonts);
            break;
        }
    }
}

pub fn launch(svc: Arc<dyn AppService>) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sirin").with_inner_size([1100.0, 740.0]).with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("Sirin", options, Box::new(move |cc| Ok(Box::new(SirinApp::new(svc, cc)))))
        .expect("Failed to run Sirin UI");
}
