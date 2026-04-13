//! Sirin UI theme — 極簡硬核風，參考 Claude Desktop settings 風格。
//!
//! 特點：零邊框卡片、大標題、淡分隔線、左側亮色條選中態。

use eframe::egui::{self, Color32, RichText, Stroke, FontFamily};

// ── Palette ──────────────────────────────────────────────────────────────────

pub const BG: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);
pub const CARD: Color32 = Color32::from_rgb(0x22, 0x22, 0x22);
pub const HOVER: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2A);
pub const BORDER: Color32 = Color32::from_rgb(0x33, 0x33, 0x33);
pub const TEXT: Color32 = Color32::from_rgb(0xE0, 0xE0, 0xE0);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x80, 0x80, 0x80);
pub const ACCENT: Color32 = Color32::from_rgb(0x00, 0xFF, 0xA3);
pub const DANGER: Color32 = Color32::from_rgb(0xFF, 0x4B, 0x4B);
pub const INFO: Color32 = Color32::from_rgb(0x4D, 0xA6, 0xFF);
pub const VALUE: Color32 = Color32::WHITE;
pub const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD9, 0x3D);

// ── Typography ───────────────────────────────────────────────────────────────

pub const FONT_TITLE: f32 = 18.0;     // section title (bold, white)
pub const FONT_HEADING: f32 = 15.0;   // sub-heading
pub const FONT_BODY: f32 = 13.0;      // normal content
pub const FONT_SMALL: f32 = 11.5;     // labels, secondary
pub const FONT_CAPTION: f32 = 10.0;   // timestamps

// ── Spacing ──────────────────────────────────────────────────────────────────

pub const SP_XS: f32 = 4.0;
pub const SP_SM: f32 = 8.0;
pub const SP_MD: f32 = 12.0;
pub const SP_LG: f32 = 20.0;    // between sections (generous)
pub const SP_XL: f32 = 32.0;    // page-level gaps

// ── Apply ────────────────────────────────────────────────────────────────────

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.faint_bg_color = CARD;
    v.extreme_bg_color = Color32::from_rgb(0x12, 0x12, 0x12);

    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.bg_stroke = Stroke::NONE;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);

    v.widgets.inactive.bg_fill = CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(0.5, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);

    v.widgets.hovered.bg_fill = HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, TEXT_DIM);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);

    v.widgets.active.bg_fill = HOVER;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, VALUE);

    v.selection.bg_fill = ACCENT.linear_multiply(0.12);
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    v.window_corner_radius = 4.0.into();
    v.menu_corner_radius = 4.0.into();
    v.popup_shadow = egui::epaint::Shadow { offset: [1, 2], blur: 6, spread: 0, color: Color32::from_black_alpha(60) };

    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(SP_SM, SP_XS);
    style.spacing.button_padding = egui::vec2(SP_SM, SP_XS);
    ctx.set_style(style);
}

// ── Widgets ──────────────────────────────────────────────────────────────────

/// Section — big bold title + thin separator below, then content.
/// Like Claude Desktop's "Plan usage limits" sections.
pub fn section(ui: &mut egui::Ui, title: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(SP_LG);
    ui.label(RichText::new(title).size(FONT_TITLE).strong().color(TEXT));
    ui.add_space(SP_SM);
    content(ui);
    ui.add_space(SP_SM);
    // Thin separator
    thin_separator(ui);
}

/// Ultra-thin separator line (#333).
pub fn thin_separator(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let pos = ui.cursor().left_top();
    ui.painter().line_segment(
        [pos, egui::pos2(pos.x + width, pos.y)],
        Stroke::new(0.5, BORDER),
    );
    ui.add_space(SP_XS);
}

/// Card — NO border. Just slightly lighter bg. Claude Desktop style.
pub fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(4.0)
        .inner_margin(SP_MD)
        .show(ui, |ui| content(ui));
    ui.add_space(SP_SM);
}

/// Pill badge.
pub fn badge(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.linear_multiply(0.12))
        .corner_radius(3.0)
        .inner_margin(egui::vec2(6.0, 1.0))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(FONT_CAPTION).color(color));
        });
}

/// Count badge.
pub fn count_badge(ui: &mut egui::Ui, count: usize) {
    if count == 0 { return; }
    egui::Frame::new()
        .fill(ACCENT)
        .corner_radius(8.0)
        .inner_margin(egui::vec2(5.0, 1.0))
        .show(ui, |ui| {
            ui.label(RichText::new(format!("{count}")).size(FONT_CAPTION).strong().color(BG));
        });
}

/// Status dot (small circle) + label text. Available for future use.
#[allow(dead_code)]
pub fn status_dot(ui: &mut egui::Ui, label: &str, ok: bool) {
    let color = if ok { ACCENT } else { DANGER };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, color);
        ui.label(RichText::new(label).size(FONT_BODY).color(TEXT));
    });
}

/// Underline tab bar.
pub fn tab_bar(ui: &mut egui::Ui, labels: &[&str], selected: &mut usize) {
    ui.horizontal(|ui| {
        for (i, label) in labels.iter().enumerate() {
            let active = *selected == i;
            let color = if active { TEXT } else { TEXT_DIM };
            let response = ui.add(
                egui::Label::new(RichText::new(*label).size(FONT_BODY).color(color))
                    .selectable(false).sense(egui::Sense::click()),
            );
            if active {
                let r = response.rect;
                ui.painter().line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(2.0, ACCENT));
            }
            if response.clicked() { *selected = i; }
            ui.add_space(SP_LG);
        }
    });
    thin_separator(ui);
    ui.add_space(SP_SM);
}

/// Key-value info row — label left (100px), value right after. Monospace values.
pub fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(120.0, ui.spacing().interact_size.y),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| { ui.label(RichText::new(label).size(FONT_BODY).color(TEXT_DIM)); },
        );
        ui.label(RichText::new(value).size(FONT_BODY).color(TEXT).family(FontFamily::Monospace));
    });
}

/// Status row — label + dot+status text.
pub fn status_row(ui: &mut egui::Ui, label: &str, status: &str, ok: bool) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(120.0, ui.spacing().interact_size.y),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| { ui.label(RichText::new(label).size(FONT_BODY).color(TEXT)); },
        );
        let color = if ok { ACCENT } else { DANGER };
        ui.colored_label(color, RichText::new(format!("● {status}")).size(FONT_SMALL));
    });
}

pub fn status_color(status: &str) -> Color32 {
    match status {
        "DONE" => ACCENT, "PENDING" | "RUNNING" => YELLOW, "FOLLOWING" => INFO,
        "FOLLOWUP_NEEDED" => YELLOW, "FAILED" | "ERROR" => DANGER, _ => TEXT_DIM,
    }
}

pub fn log_color(level: crate::ui_service::LogLevel) -> Color32 {
    use crate::ui_service::LogLevel;
    match level {
        LogLevel::Error => DANGER, LogLevel::Warn => YELLOW, LogLevel::Telegram => INFO,
        LogLevel::Research => ACCENT, LogLevel::Followup => YELLOW,
        LogLevel::Coding => Color32::from_rgb(0xBB, 0x86, 0xFC),
        LogLevel::Teams => Color32::from_rgb(0x00, 0xCC, 0xCC),
        LogLevel::Info | LogLevel::Normal => TEXT_DIM,
    }
}
