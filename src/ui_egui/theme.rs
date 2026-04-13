//! UI theme — Catppuccin Mocha + generous spacing + reusable components.
//! Design: Slack sidebar, Linear tabs, professional dark theme.
//! All sizing flows from constants here — change once, update everywhere.

use eframe::egui::{self, Color32, RichText, Stroke};

// ── Catppuccin Mocha palette ─────────────────────────────────────────────────

pub const BASE: Color32 = Color32::from_rgb(30, 30, 46);
pub const MANTLE: Color32 = Color32::from_rgb(24, 24, 37);
pub const CRUST: Color32 = Color32::from_rgb(17, 17, 27);
pub const SURFACE0: Color32 = Color32::from_rgb(49, 50, 68);
pub const SURFACE1: Color32 = Color32::from_rgb(69, 71, 90);
pub const SURFACE2: Color32 = Color32::from_rgb(88, 91, 112);
pub const OVERLAY0: Color32 = Color32::from_rgb(108, 112, 134);
pub const TEXT: Color32 = Color32::from_rgb(205, 214, 244);
pub const SUBTEXT0: Color32 = Color32::from_rgb(166, 173, 200);
pub const SUBTEXT1: Color32 = Color32::from_rgb(186, 194, 222);

pub const BLUE: Color32 = Color32::from_rgb(137, 180, 250);
pub const GREEN: Color32 = Color32::from_rgb(166, 227, 161);
pub const RED: Color32 = Color32::from_rgb(243, 139, 168);
pub const YELLOW: Color32 = Color32::from_rgb(249, 226, 175);
pub const PEACH: Color32 = Color32::from_rgb(250, 179, 135);
pub const MAUVE: Color32 = Color32::from_rgb(203, 166, 247);
pub const TEAL: Color32 = Color32::from_rgb(148, 226, 213);
pub const LAVENDER: Color32 = Color32::from_rgb(180, 190, 254);

// ── Typography ───────────────────────────────────────────────────────────────

pub const FONT_HEADING: f32 = 18.0;
pub const FONT_BODY: f32 = 14.0;
pub const FONT_SMALL: f32 = 12.0;
pub const FONT_CAPTION: f32 = 11.0;

// ── Spacing ──────────────────────────────────────────────────────────────────

pub const SP_XS: f32 = 4.0;
pub const SP_SM: f32 = 8.0;
pub const SP_MD: f32 = 12.0;
pub const SP_LG: f32 = 16.0;
pub const SP_XL: f32 = 24.0;
pub const CARD_RADIUS: f32 = 8.0;

// ── Apply ────────────────────────────────────────────────────────────────────

pub fn apply(ctx: &egui::Context) {
    catppuccin_egui::set_theme(ctx, catppuccin_egui::MOCHA);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(SP_SM, SP_XS);
    style.spacing.button_padding = egui::vec2(SP_MD, SP_SM);
    ctx.set_style(style);

    let mut v = ctx.style().visuals.clone();
    v.window_corner_radius = CARD_RADIUS.into();
    v.menu_corner_radius = 6.0.into();
    v.popup_shadow = egui::epaint::Shadow { offset: [2, 4], blur: 8, spread: 0, color: Color32::from_black_alpha(40) };
    ctx.set_visuals(v);
}

// ── Widgets ──────────────────────────────────────────────────────────────────

/// Card — rounded background, generous padding.
pub fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(SURFACE0)
        .corner_radius(CARD_RADIUS)
        .inner_margin(SP_LG) // 16px all sides
        .show(ui, |ui| content(ui));
    ui.add_space(SP_SM);
}

/// Section — title + content with left indent for visual hierarchy.
pub fn section(ui: &mut egui::Ui, title: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(SP_SM);
    ui.label(RichText::new(title).size(FONT_SMALL).strong().color(OVERLAY0));
    ui.add_space(SP_XS);
    // Indent content slightly for visual grouping
    ui.indent(title, |ui| {
        content(ui);
    });
    ui.add_space(SP_MD);
}

/// Pill badge.
pub fn badge(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.linear_multiply(0.15))
        .corner_radius(4.0)
        .inner_margin(egui::vec2(SP_SM, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(FONT_CAPTION).color(color));
        });
}

/// Notification count badge.
pub fn count_badge(ui: &mut egui::Ui, count: usize) {
    if count == 0 { return; }
    egui::Frame::new()
        .fill(PEACH)
        .corner_radius(10.0)
        .inner_margin(egui::vec2(6.0, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(format!("{count}")).size(FONT_CAPTION).strong().color(CRUST));
        });
}

/// Underline tab bar (Linear/Notion style).
pub fn tab_bar(ui: &mut egui::Ui, labels: &[&str], selected: &mut usize) {
    ui.horizontal(|ui| {
        for (i, label) in labels.iter().enumerate() {
            let active = *selected == i;
            let color = if active { BLUE } else { SUBTEXT0 };

            let response = ui.add(
                egui::Label::new(RichText::new(*label).size(FONT_BODY).color(color))
                    .selectable(false)
                    .sense(egui::Sense::click()),
            );

            if active {
                let rect = response.rect;
                ui.painter().line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    Stroke::new(2.0, BLUE),
                );
            }

            if response.clicked() { *selected = i; }
            ui.add_space(SP_LG);
        }
    });
    // Separator
    let rect = ui.max_rect();
    ui.painter().line_segment(
        [egui::pos2(rect.left(), ui.cursor().top()), egui::pos2(rect.right(), ui.cursor().top())],
        Stroke::new(1.0, SURFACE0),
    );
    ui.add_space(SP_MD);
}

/// Key-value info row.
pub fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.colored_label(OVERLAY0, RichText::new(label).size(FONT_SMALL));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).size(FONT_SMALL).color(SUBTEXT1));
        });
    });
}

/// Status dot + text row.
pub fn status_row(ui: &mut egui::Ui, label: &str, status: &str, ok: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(FONT_BODY).color(TEXT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (dot, color) = if ok { ("●", GREEN) } else { ("○", RED) };
            ui.colored_label(color, RichText::new(format!("{dot} {status}")).size(FONT_SMALL));
        });
    });
}

pub fn status_color(status: &str) -> Color32 {
    match status {
        "DONE" => GREEN, "PENDING" | "RUNNING" => YELLOW, "FOLLOWING" => BLUE,
        "FOLLOWUP_NEEDED" => PEACH, "FAILED" | "ERROR" => RED, "ROLLBACK" => MAUVE,
        _ => OVERLAY0,
    }
}

pub fn log_color(level: crate::ui_service::LogLevel) -> Color32 {
    use crate::ui_service::LogLevel;
    match level {
        LogLevel::Error => RED, LogLevel::Warn => YELLOW, LogLevel::Telegram => BLUE,
        LogLevel::Research => GREEN, LogLevel::Followup => PEACH, LogLevel::Coding => MAUVE,
        LogLevel::Teams => TEAL, LogLevel::Info | LogLevel::Normal => OVERLAY0,
    }
}
