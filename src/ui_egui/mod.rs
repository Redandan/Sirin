//! egui immediate-mode UI for Sirin.
//! 極簡硬核風 (#1A1A1A + #00FFA3) + AppService trait.

mod theme;
mod sidebar;
mod workspace;
mod settings;
mod log_view;
mod browser;
mod monitor;
mod team_panel;
mod test_dashboard;
mod coverage_panel;
mod browser_monitor;
mod mcp_playground;
mod ops_panel;
mod testing_panel;
mod automation_panel;
mod system_panel;
mod top_bar;
mod dashboard;
mod command_palette;

use std::sync::Arc;
use std::collections::VecDeque;

use eframe::egui::{self, RichText};

use crate::ui_service::*;

// ── View ─────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone)]
enum View {
    Dashboard,             // Default landing — summary + Coverage card + Browser preview
    Testing,               // Runs | Coverage | Browser (legacy sub-tabs; commit 5 split)
    Workspace(usize),      // Per-agent detail
}

/// Modal overlay opened from the palette / gear menu. Renders the existing
/// panel inside an `egui::Window` over the central panel.
#[derive(PartialEq, Clone, Copy, Default)]
enum Modal {
    #[default]
    None,
    Automation,   // Dev Squad + MCP (automation_panel::show)
    Ops,          // AI Router | Tasks | Cost (ops_panel::show)
    System,       // Settings | Logs (system_panel::show)
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

    sidebar_state:    sidebar::SidebarState,
    workspace_state:  workspace::WorkspaceState,
    dashboard_state:  dashboard::DashboardState,
    testing_state:    testing_panel::TestingPanelState,
    // Wired via command palette / gear menu (Modal overlay).
    automation_state: automation_panel::AutomationPanelState,
    ops_state:        ops_panel::OpsPanelState,
    system_state:     system_panel::SystemPanelState,
    palette_state:    command_palette::PaletteState,
    modal:            Modal,
    gear_menu_open:   bool,

    /// Has the first-run config_check been performed this session?
    /// On `false`, we run `config_check()` once and force-open the System
    /// modal (Settings tab) if any Error-severity issue is reported —
    /// covers the "missing LLM API key" case so first-time users aren't
    /// stuck with a broken dashboard.
    first_run_check_done: bool,

    /// Dismissed once per session so the banner doesn't reappear after dismiss
    update_banner_dismissed: bool,

    /// View transition tracking — show loading spinner briefly after switch
    /// for instant click feedback (covers any per-panel first-render cost).
    prev_view: View,
    view_changed_at: std::time::Instant,
}

impl SirinApp {
    pub fn new(svc: Arc<dyn AppService>, cc: &eframe::CreationContext) -> Self {
        setup_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);
        let agents = svc.list_agents();
        Self {
            svc, view: View::Dashboard, agents,
            tasks: Vec::new(),
            pending_counts: std::collections::HashMap::new(),
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            toasts: VecDeque::new(),
            renaming: None,
            sidebar_state:    Default::default(),
            workspace_state:  Default::default(),
            dashboard_state:  Default::default(),
            testing_state:    Default::default(),
            automation_state: Default::default(),
            ops_state:        Default::default(),
            system_state:     Default::default(),
            palette_state:    Default::default(),
            modal:            Modal::None,
            gear_menu_open:   false,
            first_run_check_done: false,
            update_banner_dismissed: false,
            prev_view: View::Dashboard,
            // Far in the past so initial frame doesn't show a spurious loading.
            view_changed_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
        }
    }

    fn refresh(&mut self) {
        self.agents = self.svc.list_agents();
        self.tasks = self.svc.recent_tasks(200);
        self.pending_counts.clear();
        for a in &self.agents { self.pending_counts.insert(a.id.clone(), self.svc.pending_count(&a.id)); }
        self.last_refresh = std::time::Instant::now();
    }

    /// Map a palette selection to a view switch / modal open.
    fn handle_palette_choice(&mut self, entry: command_palette::PaletteEntry) {
        use command_palette::PaletteEntry as E;
        match entry {
            E::Coverage => {
                self.view = View::Testing;
                self.testing_state.tab = testing_panel::TestingTab::Coverage;
            }
            E::Browser => {
                self.view = View::Testing;
                self.testing_state.tab = testing_panel::TestingTab::Browser;
            }
            E::DevSquad => {
                self.modal = Modal::Automation;
                self.automation_state.tab = automation_panel::AutoTab::Squad;
            }
            E::McpPlayground => {
                self.modal = Modal::Automation;
                self.automation_state.tab = automation_panel::AutoTab::Mcp;
            }
            E::AiRouter => {
                self.modal = Modal::Ops;
                self.ops_state.tab = ops_panel::OpsTab::AiRouter;
            }
            E::SessionTasks => {
                self.modal = Modal::Ops;
                self.ops_state.tab = ops_panel::OpsTab::SessionTasks;
            }
            E::CostKb => {
                self.modal = Modal::Ops;
                self.ops_state.tab = ops_panel::OpsTab::CostKb;
            }
            E::Settings => {
                self.modal = Modal::System;
                self.system_state.tab = system_panel::SysTab::Settings;
            }
            E::Logs => {
                self.modal = Modal::System;
                self.system_state.tab = system_panel::SysTab::Log;
            }
            E::GoDashboard => {
                self.view = View::Dashboard;
                self.modal = Modal::None;
            }
        }
    }

    /// Render the active modal as a centered Window over the central panel.
    fn show_modal(&mut self, ctx: &egui::Context) {
        let title = match self.modal {
            Modal::Automation => "Automation",
            Modal::Ops        => "OPS",
            Modal::System     => "System",
            Modal::None       => return,
        };

        let mut close_requested = false;
        let screen = ctx.screen_rect();
        let win_size = egui::vec2(
            (screen.width()  * 0.78).clamp(640.0, 1100.0),
            (screen.height() * 0.78).clamp(420.0,  820.0),
        );

        egui::Window::new(title)
            .collapsible(false)
            .resizable(true)
            .default_size(win_size)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(theme::SP_MD)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .corner_radius(4.0),
            )
            .show(ctx, |ui| {
                // Header with close button
                ui.horizontal(|ui| {
                    ui.colored_label(theme::TEXT_DIM,
                        RichText::new(title).size(theme::FONT_SMALL).strong().monospace());
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.add(egui::Button::new(
                                RichText::new("✕ Close").size(theme::FONT_CAPTION)
                                    .color(theme::TEXT_DIM),
                            ).frame(false)).clicked() {
                                close_requested = true;
                            }
                        },
                    );
                });
                ui.add_space(theme::SP_XS);
                theme::thin_separator(ui);
                ui.add_space(theme::SP_SM);

                match self.modal {
                    Modal::Automation => automation_panel::show(ui, &self.svc, &mut self.automation_state),
                    Modal::Ops        => ops_panel::show(ui, &self.svc, &mut self.ops_state),
                    Modal::System     => system_panel::show(ui, &self.svc, &self.agents, &mut self.system_state),
                    Modal::None       => {}
                }
            });

        // Esc closes too.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close_requested = true;
        }
        if close_requested {
            self.modal = Modal::None;
        }
    }

    /// Gear dropdown — small menu anchored top-right with quick links.
    fn show_gear_menu(&mut self, ctx: &egui::Context) {
        let mut close = false;
        egui::Area::new(egui::Id::new("gear_menu"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 36.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(theme::CARD)
                    .corner_radius(4.0)
                    .inner_margin(theme::SP_SM)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .show(ui, |ui| {
                        ui.set_min_width(180.0);
                        if menu_item(ui, "Settings").clicked() {
                            self.modal = Modal::System;
                            self.system_state.tab = system_panel::SysTab::Settings;
                            close = true;
                        }
                        if menu_item(ui, "System Logs").clicked() {
                            self.modal = Modal::System;
                            self.system_state.tab = system_panel::SysTab::Log;
                            close = true;
                        }
                        if menu_item(ui, "Open Command Palette  ⌘K").clicked() {
                            self.palette_state.open();
                            close = true;
                        }
                        ui.add_space(theme::SP_XS);
                        theme::thin_separator(ui);
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new(concat!("Sirin v", env!("CARGO_PKG_VERSION")))
                                .size(theme::FONT_CAPTION).monospace());
                    });
            });
        // Click outside closes the menu.
        if ctx.input(|i| i.pointer.any_click())
            && !ctx.is_pointer_over_area()
        {
            close = true;
        }
        if close { self.gear_menu_open = false; }
    }
}

fn menu_item(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 24.0),
        egui::Sense::click(),
    );
    if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, theme::HOVER);
    }
    ui.painter().text(
        egui::pos2(rect.left() + theme::SP_SM, rect.center().y),
        egui::Align2::LEFT_CENTER, label,
        egui::FontId::proportional(theme::FONT_SMALL), theme::TEXT,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

impl eframe::App for SirinApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_refresh.elapsed() > std::time::Duration::from_secs(5) { self.refresh(); }
        ctx.request_repaint_after(std::time::Duration::from_secs(5));

        // ── First-run config check: force Settings modal on missing LLM key ─
        if !self.first_run_check_done {
            self.first_run_check_done = true;
            let issues = self.svc.config_check();
            let has_error = issues.iter().any(|i| i.severity == ConfigSeverity::Error);
            if has_error {
                self.modal = Modal::System;
                self.system_state.tab = system_panel::SysTab::Settings;
                self.toasts.push_back(Toast::from_event(ToastEvent {
                    level: ToastLevel::Error,
                    text: "設定有錯誤 — 請先在 Settings 修正".into(),
                }));
            }
        }

        // Deactivate screenshot pump when Testing panel is not on Browser tab
        if !matches!(self.view, View::Testing) {
            if let Some(ms) = crate::monitor::state() {
                ms.set_view_active(false);
            }
        }

        for te in self.svc.poll_toasts() { self.toasts.push_back(Toast::from_event(te)); }
        let now = std::time::Instant::now();
        self.toasts.retain(|t| t.expires > now);

        // ── Global Ctrl/Cmd+K shortcut ──────────────────────────────────
        // Listen on the *first* update of each frame so the palette opens
        // even when focus is in another widget. We avoid consuming events
        // because TextEdit may also need them — we just observe.
        let palette_shortcut = ctx.input(|i| {
            (i.modifiers.ctrl || i.modifiers.command || i.modifiers.mac_cmd)
                && i.key_pressed(egui::Key::K)
        });
        if palette_shortcut && !self.palette_state.open {
            self.palette_state.open();
        }

        // ── Top status bar (32pt, always visible above sidebar/central) ──
        let top_action = top_bar::show(ctx, &self.svc);
        match top_action {
            top_bar::TopBarAction::OpenPalette => self.palette_state.open(),
            top_bar::TopBarAction::OpenGearMenu => self.gear_menu_open = !self.gear_menu_open,
            top_bar::TopBarAction::None => {}
        }

        sidebar::show(ctx, &self.svc, &self.agents, &self.pending_counts, &mut self.view, &mut self.renaming, &mut self.sidebar_state);

        // ── Detect view switch → start loading window ─────────────────────
        // The sidebar may have just mutated `self.view`. If so, start a brief
        // loading window so the click feels instant and we mask any per-panel
        // first-frame layout cost. Re-paint after the window so it clears.
        const LOADING_MS: u64 = 220;
        if self.view != self.prev_view {
            self.prev_view = self.view.clone();
            self.view_changed_at = std::time::Instant::now();
            ctx.request_repaint_after(std::time::Duration::from_millis(LOADING_MS + 30));
        }
        let loading = self.view_changed_at.elapsed() < std::time::Duration::from_millis(LOADING_MS);

        // ── Update banner (shown when a new version is available) ────────
        if !self.update_banner_dismissed {
            show_update_banner(ctx, &mut self.update_banner_dismissed);
        }

        // ── Central panel ────────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::vec2(theme::SP_XL, theme::SP_LG)))
            .show(ctx, |ui| {
                if loading {
                    show_loading(ui);
                } else {
                    match self.view.clone() {
                        View::Dashboard => {
                            let act = dashboard::show(ui, &self.svc, &mut self.dashboard_state);
                            match act {
                                dashboard::DashboardAction::OpenTesting => {
                                    self.view = View::Testing;
                                }
                                dashboard::DashboardAction::OpenTestingCoverage => {
                                    self.view = View::Testing;
                                    self.testing_state.tab = testing_panel::TestingTab::Coverage;
                                }
                                dashboard::DashboardAction::OpenTestingBrowser => {
                                    self.view = View::Testing;
                                    self.testing_state.tab = testing_panel::TestingTab::Browser;
                                }
                                dashboard::DashboardAction::None => {}
                            }
                        }
                        View::Testing        => testing_panel::show(ui, &self.svc, &mut self.testing_state),
                        View::Workspace(idx) => workspace::show(ui, &self.svc, &self.agents, idx, &self.tasks, &self.pending_counts, &mut self.workspace_state),
                    }
                }
            });

        // ── Modal overlay (shown above central panel, below palette) ─────
        if self.modal != Modal::None {
            self.show_modal(ctx);
        }

        // ── Gear menu (anchored under the gear icon in the top bar) ──────
        if self.gear_menu_open {
            self.show_gear_menu(ctx);
        }

        // ── Command palette (top-most overlay) ───────────────────────────
        if let Some(entry) = command_palette::show(ctx, &mut self.palette_state) {
            self.handle_palette_choice(entry);
            self.palette_state.close();
        }

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

/// Centered spinner overlay shown briefly after a view switch. Provides
/// instant click feedback and masks any first-render layout cost in the
/// destination panel (mostly the agent_detail YAML cache miss + workspace
/// table layout).
fn show_loading(ui: &mut egui::Ui) {
    let avail = ui.available_size();
    ui.vertical_centered(|ui| {
        ui.add_space((avail.y * 0.38).max(40.0));
        ui.add(egui::Spinner::new().size(28.0).color(theme::ACCENT));
        ui.add_space(theme::SP_SM);
        ui.colored_label(theme::TEXT_DIM,
            RichText::new("載入中…").size(theme::FONT_CAPTION).monospace());
    });
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

// ── Update banner ─────────────────────────────────────────────────────────────

/// Renders a slim banner when an update is available / applying / done.
/// Sets `dismissed = true` when the user clicks ✕.
fn show_update_banner(ctx: &egui::Context, dismissed: &mut bool) {
    use crate::updater::{UpdateStatus, apply_update};

    let status = crate::updater::get_status();

    // Only show for actionable states. `show_download_btn` toggles the
    // 📥 fallback (opens GitHub Releases in the default browser) — used both
    // for the proactive "available" state and the failure-recovery state.
    let (msg, accent, show_apply_btn, show_download_btn, version) = match &status {
        UpdateStatus::Available(v) => (
            format!("🆕  Sirin v{v} 可用"),
            egui::Color32::from_rgb(0, 160, 80),
            true,
            true,
            Some(v.clone()),
        ),
        UpdateStatus::Applying => (
            "⏳  下載更新中…".into(),
            egui::Color32::from_rgb(80, 130, 200),
            false,
            false,
            None,
        ),
        UpdateStatus::RestartRequired => (
            "✅  更新完成 — 重啟 Sirin 生效".into(),
            egui::Color32::from_rgb(0, 200, 100),
            false,
            false,
            None,
        ),
        UpdateStatus::ApplyFailed(e) => (
            format!("❌  更新失敗：{e}"),
            egui::Color32::from_rgb(200, 60, 60),
            false,
            true, // show 📥 escape hatch
            None,
        ),
        _ => return, // Idle / Checking / UpToDate — nothing to show
    };

    // Failed-state banner needs more vertical room — multi-line error text.
    let banner_height = if matches!(status, UpdateStatus::ApplyFailed(_)) { 56.0 } else { 28.0 };

    egui::TopBottomPanel::top("update_banner")
        .exact_height(banner_height)
        .frame(
            egui::Frame::new()
                .fill(accent.linear_multiply(0.18))
                .inner_margin(egui::vec2(12.0, 4.0))
                .stroke(egui::Stroke::new(0.0, egui::Color32::TRANSPARENT)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(accent, RichText::new(&msg).size(12.0));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").clicked() {
                        *dismissed = true;
                    }
                    if show_download_btn {
                        // Open GitHub Releases in the default browser — works
                        // even when self-update is blocked by perms.
                        if ui.add(egui::Button::new(
                            RichText::new("📥 手動下載").size(11.0).color(accent)
                        ).frame(false)).clicked() {
                            let url = crate::updater::release_page_url();
                            // Best-effort; ignore failure (no display, etc.).
                            #[cfg(target_os = "windows")]
                            {
                                use crate::platform::NoWindow;
                                let _ = std::process::Command::new("cmd")
                                    .no_window()
                                    .args(["/C", "start", "", &url]).spawn();
                            }
                            #[cfg(target_os = "macos")]
                            let _ = std::process::Command::new("open").arg(&url).spawn();
                            #[cfg(all(unix, not(target_os = "macos")))]
                            let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                        }
                    }
                    if show_apply_btn {
                        if let Some(ref v) = version {
                            let v_clone = v.clone();
                            if ui.add(egui::Button::new(
                                RichText::new("立即更新").size(11.0).color(accent)
                            ).frame(false)).clicked() {
                                std::thread::spawn(move || { let _ = apply_update(&v_clone); });
                            }
                        }
                    }
                });
            });
        });
}

pub fn launch(svc: Arc<dyn AppService>) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(concat!("Sirin v", env!("CARGO_PKG_VERSION"))).with_inner_size([1100.0, 740.0]).with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(concat!("Sirin v", env!("CARGO_PKG_VERSION")), options, Box::new(move |cc| Ok(Box::new(SirinApp::new(svc, cc)))))
        .expect("Failed to run Sirin UI");
}
