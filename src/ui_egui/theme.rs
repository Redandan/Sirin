//! UI theme — semantic colours, spacing constants, and reusable widget helpers.
//!
//! AI reads this to know: what colours mean what, and what card/badge look like.
//! All colour values come from the Catppuccin Mocha palette.

use eframe::egui::{self, Color32, RichText, Stroke};

// ── Catppuccin Mocha semantic aliases ────────────────────────────────────────

pub const BASE: Color32 = Color32::from_rgb(30, 30, 46);       // main background
pub const MANTLE: Color32 = Color32::from_rgb(24, 24, 37);     // sidebar / panel bg
pub const CRUST: Color32 = Color32::from_rgb(17, 17, 27);      // deepest bg
pub const SURFACE0: Color32 = Color32::from_rgb(49, 50, 68);   // card bg
pub const SURFACE1: Color32 = Color32::from_rgb(69, 71, 90);   // hover bg
pub const SURFACE2: Color32 = Color32::from_rgb(88, 91, 112);  // border
pub const OVERLAY0: Color32 = Color32::from_rgb(108, 112, 134); // muted text
pub const TEXT: Color32 = Color32::from_rgb(205, 214, 244);     // primary text
pub const SUBTEXT0: Color32 = Color32::from_rgb(166, 173, 200); // secondary text
pub const SUBTEXT1: Color32 = Color32::from_rgb(186, 194, 222); // subtitle

// Accent colours
pub const BLUE: Color32 = Color32::from_rgb(137, 180, 250);
pub const GREEN: Color32 = Color32::from_rgb(166, 227, 161);
pub const RED: Color32 = Color32::from_rgb(243, 139, 168);
pub const YELLOW: Color32 = Color32::from_rgb(249, 226, 175);
pub const PEACH: Color32 = Color32::from_rgb(250, 179, 135);
pub const MAUVE: Color32 = Color32::from_rgb(203, 166, 247);
pub const TEAL: Color32 = Color32::from_rgb(148, 226, 213);
pub const LAVENDER: Color32 = Color32::from_rgb(180, 190, 254);



// ── Spacing ──────────────────────────────────────────────────────────────────

pub const GAP_SM: f32 = 4.0;
pub const GAP_MD: f32 = 8.0;
pub const GAP_LG: f32 = 12.0;
pub const GAP_XL: f32 = 16.0;
pub const CARD_RADIUS: f32 = 8.0;
pub const BADGE_RADIUS: f32 = 10.0;

// ── Visuals setup ────────────────────────────────────────────────────────────

pub fn apply(ctx: &egui::Context) {
    catppuccin_egui::set_theme(ctx, catppuccin_egui::MOCHA);

    // Fine-tune on top of catppuccin
    let mut v = ctx.style().visuals.clone();
    v.window_corner_radius = CARD_RADIUS.into();
    v.menu_corner_radius = 6.0.into();
    v.popup_shadow = egui::epaint::Shadow { offset: [2, 4], blur: 8, spread: 0, color: Color32::from_black_alpha(60) };
    ctx.set_visuals(v);
}

// ── Reusable widgets ─────────────────────────────────────────────────────────

/// A rounded card container with consistent padding and border.
pub fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(SURFACE0)
        .corner_radius(CARD_RADIUS)
        .inner_margin(GAP_LG)
        .stroke(Stroke::new(1.0, SURFACE2.linear_multiply(0.3)))
        .show(ui, |ui| content(ui));
    ui.add_space(GAP_MD);
}

/// A section with a title label and card body.
pub fn section(ui: &mut egui::Ui, title: &str, content: impl FnOnce(&mut egui::Ui)) {
    card(ui, |ui| {
        ui.label(RichText::new(title).strong().small().color(OVERLAY0));
        ui.add_space(GAP_SM);
        content(ui);
    });
}

/// A coloured status badge (pill shape).
pub fn badge(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.linear_multiply(0.15))
        .corner_radius(BADGE_RADIUS)
        .inner_margin(egui::vec2(6.0, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(label).small().color(color));
        });
}

/// A notification count badge (orange pill).
pub fn count_badge(ui: &mut egui::Ui, count: usize) {
    if count == 0 { return; }
    egui::Frame::new()
        .fill(PEACH)
        .corner_radius(BADGE_RADIUS)
        .inner_margin(egui::vec2(5.0, 1.0))
        .show(ui, |ui| {
            ui.label(RichText::new(format!("{count}")).small().strong().color(CRUST));
        });
}

/// A key-value row for settings/detail views.
pub fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.colored_label(OVERLAY0, RichText::new(label).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).small().color(SUBTEXT1));
        });
    });
}

/// A connection status row with coloured dot.
pub fn status_row(ui: &mut egui::Ui, label: &str, status: &str, ok: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(TEXT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (dot, color) = if ok { ("●", GREEN) } else { ("○", RED) };
            ui.colored_label(color, format!("{dot} {status}"));
        });
    });
}

/// Map a task status string to a Catppuccin accent colour.
pub fn status_color(status: &str) -> Color32 {
    match status {
        "DONE" => GREEN,
        "PENDING" | "RUNNING" => YELLOW,
        "FOLLOWING" => BLUE,
        "FOLLOWUP_NEEDED" => PEACH,
        "FAILED" | "ERROR" => RED,
        "ROLLBACK" => MAUVE,
        _ => OVERLAY0,
    }
}

/// Map a log level to a Catppuccin colour.
pub fn log_color(level: crate::ui_service::LogLevel) -> Color32 {
    use crate::ui_service::LogLevel;
    match level {
        LogLevel::Error => RED,
        LogLevel::Warn => YELLOW,
        LogLevel::Telegram => BLUE,
        LogLevel::Research => GREEN,
        LogLevel::Followup => PEACH,
        LogLevel::Coding => MAUVE,
        LogLevel::Teams => TEAL,
        LogLevel::Info | LogLevel::Normal => OVERLAY0,
    }
}
