//! egui immediate-mode UI for Sirin.
//! 極簡硬核風 (#1A1A1A + #00FFA3) + AppService trait.

mod theme;
mod sidebar;
mod workspace;
mod settings;
mod log_view;
mod workflow;
mod meeting;
mod browser;
mod monitor;

use std::sync::Arc;
use std::collections::VecDeque;

use eframe::egui::{self, RichText};

use crate::ui_service::*;

// ── View ─────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone)]
enum View { Workspace(usize), Settings, Log, Workflow, Meeting, Browser, Monitor }

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
    browser_state: browser::BrowserUiState,
    monitor_state: monitor::MonitorViewState,
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
            meeting_state: Default::default(), browser_state: Default::default(),
            monitor_state: Default::default(),
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

        // Deactivate screenshot pump when Monitor view is not open
        if !matches!(self.view, View::Monitor) {
            if let Some(ms) = crate::monitor::state() {
                ms.set_view_active(false);
            }
        }

        for te in self.svc.poll_toasts() { self.toasts.push_back(Toast::from_event(te)); }
        let now = std::time::Instant::now();
        self.toasts.retain(|t| t.expires > now);

        sidebar::show(ctx, &self.svc, &self.agents, &self.pending_counts, &mut self.view, &mut self.renaming);

        // ── Top bar — shows current page context ─────────────────────────
        egui::TopBottomPanel::top("top_bar")
            .exact_height(34.0)
            .frame(egui::Frame::new().fill(theme::BG)
                .inner_margin(egui::vec2(theme::SP_XL, theme::SP_SM))
                .stroke(egui::Stroke::new(0.5, theme::BORDER)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Page context
                    let title = match &self.view {
                        View::Workspace(idx) => {
                            let name = self.agents.get(*idx).map(|a| a.name.as_str()).unwrap_or("—");
                            name.to_string()
                        }
                        View::Settings => "系統設定".into(),
                        View::Log => "系統 Log".into(),
                        View::Workflow => "Skill 開發".into(),
                        View::Meeting => "會議室".into(),
                        View::Browser => "Browser".into(),
                        View::Monitor => "Monitor".into(),
                    };
                    ui.label(RichText::new(&title).size(theme::FONT_HEADING).strong().color(theme::TEXT));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let total_pending: usize = self.pending_counts.values().sum();
                        if total_pending > 0 {
                            theme::count_badge(ui, total_pending);
                            ui.add_space(theme::SP_XS);
                            ui.colored_label(theme::TEXT_DIM, RichText::new("待審").size(theme::FONT_CAPTION));
                        }
                    });
                });
            });

        // ── Central panel ────────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::vec2(theme::SP_XL, theme::SP_LG)))
            .show(ctx, |ui| {
                match self.view.clone() {
                    View::Workspace(idx) => workspace::show(ui, &self.svc, &self.agents, idx, &self.tasks, &self.pending_counts, &mut self.workspace_state),
                    View::Settings => settings::show(ui, &self.svc, &self.agents, &mut self.settings_state),
                    View::Log => log_view::show(ui, &self.svc, &mut self.log_state),
                    View::Workflow => workflow::show(ui, &self.svc, &mut self.workflow_state),
                    View::Meeting => meeting::show(ui, &self.svc, &self.agents, &mut self.meeting_state),
                    View::Browser => browser::show(ui, &self.svc, &mut self.browser_state),
                    View::Monitor => monitor::show(ui, &mut self.monitor_state),
                }
            });

        // Toast overlay
        if !self.toasts.is_empty() {
            egui::Area::new(egui::Id::new("toasts"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
                .show(ctx, |ui| {
                    for toast in self.toasts.iter().rev().take(3) {
                        let (fg, bg, icon) = match toast.level {
                            ToastLevel::Success => (theme::ACCENT, theme::ACCENT.linear_multiply(0.12), "✓"),
                            ToastLevel::Error => (theme::DANGER, theme::DANGER.linear_multiply(0.12), "✗"),
                            ToastLevel::Info => (theme::INFO, theme::INFO.linear_multiply(0.12), "ℹ"),
                        };
                        egui::Frame::new().fill(bg).corner_radius(4.0)
                            .inner_margin(theme::SP_MD)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.colored_label(fg, RichText::new(icon).strong());
                                    ui.colored_label(fg, &toast.text);
                                });
                            });
                        ui.add_space(theme::SP_XS);
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
