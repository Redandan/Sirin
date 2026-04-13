//! egui immediate-mode UI for Sirin.
//! All backend access goes through AppService trait.

mod sidebar;
mod workspace;
mod settings;
mod log_view;
mod workflow;
mod meeting;

use std::sync::Arc;
use std::collections::VecDeque;

use eframe::egui::{self, Color32, RichText, ScrollArea};

use crate::ui_service::*;

// ── View selector ────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone)]
enum View {
    Workspace(usize),
    Settings,
    Log,
    Workflow,
    Meeting,
}

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
    fn fg(&self) -> Color32 {
        match self.level {
            ToastLevel::Success => Color32::from_rgb(80, 200, 120),
            ToastLevel::Error => Color32::from_rgb(220, 80, 80),
            ToastLevel::Info => Color32::from_rgb(160, 200, 255),
        }
    }
    fn bg(&self) -> Color32 {
        match self.level {
            ToastLevel::Success => Color32::from_rgb(18, 50, 28),
            ToastLevel::Error => Color32::from_rgb(60, 18, 18),
            ToastLevel::Info => Color32::from_rgb(20, 35, 60),
        }
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

    log_state: log_view::LogState,
    workspace_state: workspace::WorkspaceState,
    settings_state: settings::SettingsState,
    workflow_state: workflow::WorkflowUiState,
    meeting_state: meeting::MeetingState,
}

impl SirinApp {
    pub fn new(svc: Arc<dyn AppService>, cc: &eframe::CreationContext) -> Self {
        setup_fonts(&cc.egui_ctx);
        let agents = svc.list_agents();
        Self {
            svc, view: View::Workspace(0), agents,
            tasks: Vec::new(),
            pending_counts: std::collections::HashMap::new(),
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            toasts: VecDeque::new(),
            log_state: log_view::LogState::default(),
            workspace_state: workspace::WorkspaceState::default(),
            settings_state: settings::SettingsState::default(),
            workflow_state: workflow::WorkflowUiState::default(),
            meeting_state: meeting::MeetingState::default(),
        }
    }

    fn refresh(&mut self) {
        self.agents = self.svc.list_agents();
        self.tasks = self.svc.recent_tasks(200);
        self.pending_counts.clear();
        for a in &self.agents {
            self.pending_counts.insert(a.id.clone(), self.svc.pending_count(&a.id));
        }
        self.last_refresh = std::time::Instant::now();
    }
}

impl eframe::App for SirinApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Auto-refresh
        if self.last_refresh.elapsed() > std::time::Duration::from_secs(5) {
            self.refresh();
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(5));

        // Drain toast events from service
        for te in self.svc.poll_toasts() {
            self.toasts.push_back(Toast::from_event(te));
        }
        // Expire old toasts
        let now = std::time::Instant::now();
        self.toasts.retain(|t| t.expires > now);

        // Sidebar
        sidebar::show(ctx, &self.svc, &self.agents, &self.pending_counts, &mut self.view);

        // Central panel
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.view.clone() {
                View::Workspace(idx) => workspace::show(ui, &self.svc, &self.agents, idx, &self.tasks, &self.pending_counts, &mut self.workspace_state),
                View::Settings => settings::show(ui, &self.svc, &self.agents, &mut self.settings_state),
                View::Log => log_view::show(ui, &self.svc, &mut self.log_state),
                View::Workflow => workflow::show(ui, &self.svc, &mut self.workflow_state),
                View::Meeting => meeting::show(ui, &self.svc, &self.agents, &mut self.meeting_state),
            }
        });

        // Toast overlay (bottom-right)
        if !self.toasts.is_empty() {
            egui::Area::new(egui::Id::new("toasts"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-10.0, -10.0))
                .show(ctx, |ui| {
                    for toast in self.toasts.iter().rev().take(3) {
                        egui::Frame::new()
                            .fill(toast.bg())
                            .corner_radius(6.0)
                            .inner_margin(egui::vec2(12.0, 6.0))
                            .show(ui, |ui| {
                                ui.colored_label(toast.fg(), &toast.text);
                            });
                        ui.add_space(4.0);
                    }
                });
        }
    }
}

// ── Font ─────────────────────────────────────────────────────────────────────

fn setup_fonts(ctx: &egui::Context) {
    let paths = if cfg!(target_os = "windows") { vec!["C:\\Windows\\Fonts\\msjh.ttc", "C:\\Windows\\Fonts\\msyh.ttc"] }
    else if cfg!(target_os = "macos") { vec!["/System/Library/Fonts/PingFang.ttc"] }
    else { vec!["/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"] };

    for path in paths {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".to_string(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "cjk".to_string());
            fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".to_string());
            ctx.set_fonts(fonts);
            break;
        }
    }
}

pub fn launch(svc: Arc<dyn AppService>) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sirin")
            .with_inner_size([1100.0, 740.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("Sirin", options, Box::new(move |cc| Ok(Box::new(SirinApp::new(svc, cc)))))
        .expect("Failed to run Sirin UI");
}
