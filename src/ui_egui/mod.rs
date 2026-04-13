//! egui immediate-mode UI for Sirin.
//!
//! All backend access goes through [`AppService`] trait — zero direct
//! imports of backend modules. AI can read this code to "see" the UI.

mod sidebar;
mod workspace;
mod settings;
mod log_view;

use std::sync::Arc;

use eframe::egui::{self, Color32, RichText, ScrollArea};

use crate::ui_service::*;

// ── View selector ────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone)]
enum View {
    Workspace(usize),
    Settings,
    Log,
}

// ── App state ────────────────────────────────────────────────────────────────

pub struct SirinApp {
    svc: Arc<dyn AppService>,
    view: View,

    // Cached data (refreshed every 5s)
    agents: Vec<AgentSummary>,
    tasks: Vec<TaskView>,
    pending_counts: std::collections::HashMap<String, usize>,

    // Refresh timer
    last_refresh: std::time::Instant,

    // Sub-view state
    log_state: log_view::LogState,
    workspace_state: workspace::WorkspaceState,
    settings_state: settings::SettingsState,
}

impl SirinApp {
    pub fn new(svc: Arc<dyn AppService>, cc: &eframe::CreationContext) -> Self {
        setup_fonts(&cc.egui_ctx);
        let agents = svc.list_agents();
        Self {
            svc,
            view: View::Workspace(0),
            agents,
            tasks: Vec::new(),
            pending_counts: std::collections::HashMap::new(),
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            log_state: log_view::LogState::default(),
            workspace_state: workspace::WorkspaceState::default(),
            settings_state: settings::SettingsState::default(),
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
        // Auto-refresh every 5 seconds (background thread avoids blocking).
        if self.last_refresh.elapsed() > std::time::Duration::from_secs(5) {
            self.refresh();
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(5));

        // ── Sidebar ──────────────────────────────────────────────────────────
        sidebar::show(ctx, &self.agents, &self.pending_counts, &mut self.view);

        // ── Central panel ────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            match &self.view.clone() {
                View::Workspace(idx) => {
                    workspace::show(ui, &self.svc, &self.agents, *idx, &self.tasks, &self.pending_counts, &mut self.workspace_state);
                }
                View::Settings => {
                    settings::show(ui, &self.svc, &self.agents, &mut self.settings_state);
                }
                View::Log => {
                    log_view::show(ui, &self.svc, &mut self.log_state);
                }
            }
        });
    }
}

// ── Font setup ───────────────────────────────────────────────────────────────

fn setup_fonts(ctx: &egui::Context) {
    let font_paths = if cfg!(target_os = "windows") {
        vec!["C:\\Windows\\Fonts\\msjh.ttc", "C:\\Windows\\Fonts\\msyh.ttc"]
    } else if cfg!(target_os = "macos") {
        vec!["/System/Library/Fonts/PingFang.ttc"]
    } else {
        vec!["/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"]
    };

    for path in font_paths {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_string(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_string());
            fonts.families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_string());
            ctx.set_fonts(fonts);
            break;
        }
    }
}

// ── Launcher ─────────────────────────────────────────────────────────────────

pub fn launch(svc: Arc<dyn AppService>) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sirin")
            .with_inner_size([1100.0, 740.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Sirin",
        options,
        Box::new(move |cc| Ok(Box::new(SirinApp::new(svc, cc)))),
    )
    .expect("Failed to run Sirin UI");
}
