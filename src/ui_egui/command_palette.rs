//! ⌘K Command Palette — overlay for accessing secondary features that the
//! sidebar and top-bar don't surface directly.
//!
//! Triggered by Ctrl+K / Cmd+K (handled in mod.rs) or by clicking the ⌘K hint
//! in the top bar. Press Esc to close. Arrow keys navigate, Enter selects.

use eframe::egui::{self, RichText};
use super::theme;

/// One entry in the palette. The action is interpreted by the parent app.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaletteEntry {
    // Testing-tab targets (handled by switching View + setting tab)
    Coverage,
    Browser,
    // Modal targets — automation_panel sub-tabs
    DevSquad,
    McpPlayground,
    // Modal targets — ops_panel sub-tabs
    AiRouter,
    SessionTasks,
    CostKb,
    // Modal targets — system_panel sub-tabs
    Settings,
    Logs,
    // Quick action — focus the dashboard
    GoDashboard,
}

impl PaletteEntry {
    pub const ALL: &'static [Self] = &[
        Self::Coverage, Self::Browser, Self::DevSquad, Self::McpPlayground,
        Self::AiRouter, Self::SessionTasks, Self::CostKb,
        Self::Settings, Self::Logs, Self::GoDashboard,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Coverage      => "Coverage Map",
            Self::Browser       => "Browser Monitor",
            Self::DevSquad      => "Dev Squad",
            Self::McpPlayground => "MCP Playground",
            Self::AiRouter      => "AI Router",
            Self::SessionTasks  => "Session & Tasks",
            Self::CostKb        => "Cost & KB Stats",
            Self::Settings      => "Settings",
            Self::Logs          => "System Logs",
            Self::GoDashboard   => "Go to Dashboard",
        }
    }

    pub fn group(&self) -> &'static str {
        match self {
            Self::Coverage | Self::Browser => "TESTING",
            Self::DevSquad | Self::McpPlayground => "AUTOMATION",
            Self::AiRouter | Self::SessionTasks | Self::CostKb => "OPS",
            Self::Settings | Self::Logs => "SYSTEM",
            Self::GoDashboard => "VIEW",
        }
    }

    pub fn keywords(&self) -> &'static str {
        match self {
            Self::Coverage      => "test feature map gap script",
            Self::Browser       => "chrome cdp screenshot viewport",
            Self::DevSquad      => "team pm engineer tester github queue",
            Self::McpPlayground => "tools external mcp",
            Self::AiRouter      => "llm route benchmark intent gemini deepseek",
            Self::SessionTasks  => "task save point todo",
            Self::CostKb        => "cost token spend kb knowledge base",
            Self::Settings      => "config persona llm api key telegram",
            Self::Logs          => "log error warn",
            Self::GoDashboard   => "home main",
        }
    }
}

#[derive(Default)]
pub struct PaletteState {
    pub open:   bool,
    pub query:  String,
    pub idx:    usize,
}

impl PaletteState {
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.idx = 0;
    }
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.idx = 0;
    }
}

/// Render the palette if open. Returns `Some(entry)` when the user selects.
/// The parent should react and then close the palette via `state.close()`.
pub fn show(ctx: &egui::Context, state: &mut PaletteState) -> Option<PaletteEntry> {
    if !state.open { return None; }

    // ESC closes.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.close();
        return None;
    }

    // Filter entries by query.
    let q = state.query.to_lowercase();
    let filtered: Vec<PaletteEntry> = PaletteEntry::ALL.iter().copied()
        .filter(|e| {
            q.is_empty()
                || e.label().to_lowercase().contains(&q)
                || e.keywords().to_lowercase().contains(&q)
                || e.group().to_lowercase().contains(&q)
        })
        .collect();

    // Clamp index.
    if filtered.is_empty() {
        state.idx = 0;
    } else if state.idx >= filtered.len() {
        state.idx = filtered.len() - 1;
    }

    // Arrow keys + Enter.
    let nav_up    = ctx.input(|i| i.key_pressed(egui::Key::ArrowUp));
    let nav_down  = ctx.input(|i| i.key_pressed(egui::Key::ArrowDown));
    let confirm   = ctx.input(|i| i.key_pressed(egui::Key::Enter));
    if nav_down && !filtered.is_empty() {
        state.idx = (state.idx + 1) % filtered.len();
    }
    if nav_up && !filtered.is_empty() {
        state.idx = if state.idx == 0 { filtered.len() - 1 } else { state.idx - 1 };
    }

    let mut chosen: Option<PaletteEntry> = None;
    if confirm {
        chosen = filtered.get(state.idx).copied();
    }

    // ── Backdrop (dimmed full-screen click-catcher) ────────────────────
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new("palette_backdrop"))
        .fixed_pos(screen.min)
        .order(egui::Order::Background)
        .show(ctx, |ui| {
            let resp = ui.allocate_response(screen.size(), egui::Sense::click());
            ui.painter().rect_filled(screen, 0.0,
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140));
            if resp.clicked() { /* close handled below */ }
        });
    // Click outside the popup → close.
    if ctx.input(|i| i.pointer.any_click())
        && !ctx.is_pointer_over_area()
    {
        // No-op: the popup `Area` will register hover; the backdrop catches outside clicks.
    }

    // ── Popup ──────────────────────────────────────────────────────────
    egui::Area::new(egui::Id::new("palette_popup"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(theme::CARD)
                .corner_radius(6.0)
                .inner_margin(theme::SP_MD)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .show(ui, |ui| {
                    ui.set_width(440.0);

                    // Header
                    ui.horizontal(|ui| {
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new("⌘K").size(theme::FONT_CAPTION).monospace());
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new("Command Palette").size(theme::FONT_SMALL));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.colored_label(theme::TEXT_DIM,
                                    RichText::new("Esc").size(theme::FONT_CAPTION).monospace());
                            },
                        );
                    });
                    ui.add_space(theme::SP_XS);

                    // Search input — auto-focus
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut state.query)
                            .desired_width(f32::INFINITY)
                            .hint_text("Type to filter…")
                            .font(egui::TextStyle::Body),
                    );
                    if !resp.has_focus() {
                        resp.request_focus();
                    }
                    ui.add_space(theme::SP_SM);
                    theme::thin_separator(ui);
                    ui.add_space(theme::SP_XS);

                    if filtered.is_empty() {
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new("No matches").size(theme::FONT_CAPTION));
                        return;
                    }

                    // Entries
                    egui::ScrollArea::vertical()
                        .id_salt("palette_list")
                        .max_height(360.0)
                        .show(ui, |ui| {
                            for (i, entry) in filtered.iter().enumerate() {
                                if entry_row(ui, *entry, i == state.idx).clicked() {
                                    chosen = Some(*entry);
                                }
                            }
                        });
                });
        });

    chosen
}

fn entry_row(ui: &mut egui::Ui, entry: PaletteEntry, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 28.0),
        egui::Sense::click(),
    );
    if active || resp.hovered() {
        ui.painter().rect_filled(rect, 4.0,
            if active { theme::HOVER } else { theme::BG.linear_multiply(0.5) });
    }
    if active {
        let bar = egui::Rect::from_min_size(
            rect.left_top(), egui::vec2(3.0, rect.height()),
        );
        ui.painter().rect_filled(bar, 1.0, theme::ACCENT);
    }

    let text_color = if active { theme::TEXT } else { theme::TEXT_DIM };
    // Group tag (left, fixed width)
    ui.painter().text(
        egui::pos2(rect.left() + theme::SP_MD, rect.center().y),
        egui::Align2::LEFT_CENTER,
        entry.group(),
        egui::FontId::monospace(theme::FONT_CAPTION),
        theme::TEXT_DIM,
    );
    // Label
    ui.painter().text(
        egui::pos2(rect.left() + 110.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        entry.label(),
        egui::FontId::proportional(theme::FONT_BODY),
        text_color,
    );

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
