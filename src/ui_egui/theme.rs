//! Sirin UI theme — 極簡硬核風 (crypto terminal / system monitor aesthetic).
//!
//! Design spec:
//!   背景 #1A1A1A, 卡片 #222222, 邊框 #333333
//!   運行 #00FFA3, 警告 #FF4B4B, 資訊 #4DA6FF
//!   主文字 #E0E0E0, 副文字 #808080, 數值 #FFFFFF mono

use eframe::egui::{self, Color32, RichText, Stroke, FontFamily};

// ── Palette ──────────────────────────────────────────────────────────────────

pub const BG: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);         // 背景
pub const CARD: Color32 = Color32::from_rgb(0x22, 0x22, 0x22);       // 卡片/面板
pub const HOVER: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2A);      // Hover 狀態
pub const BORDER: Color32 = Color32::from_rgb(0x33, 0x33, 0x33);     // 邊框
pub const TEXT: Color32 = Color32::from_rgb(0xE0, 0xE0, 0xE0);       // 主文字
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x80, 0x80, 0x80);   // 副文字/標籤
pub const ACCENT: Color32 = Color32::from_rgb(0x00, 0xFF, 0xA3);     // 運行/安全
pub const DANGER: Color32 = Color32::from_rgb(0xFF, 0x4B, 0x4B);     // 警告/停用
pub const INFO: Color32 = Color32::from_rgb(0x4D, 0xA6, 0xFF);       // 資訊/連結
pub const VALUE: Color32 = Color32::WHITE;                             // 數值 (mono)
pub const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD9, 0x3D);     // 等待/警示

// ── Typography ───────────────────────────────────────────────────────────────

pub const FONT_HEADING: f32 = 16.0;
pub const FONT_BODY: f32 = 13.0;
pub const FONT_SMALL: f32 = 11.5;
pub const FONT_CAPTION: f32 = 10.0;

// ── Spacing ──────────────────────────────────────────────────────────────────

pub const SP_XS: f32 = 4.0;
pub const SP_SM: f32 = 8.0;
pub const SP_MD: f32 = 12.0;
pub const SP_LG: f32 = 16.0;
pub const SP_XL: f32 = 24.0;

// ── Apply theme ──────────────────────────────────────────────────────────────

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    // Backgrounds
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.faint_bg_color = CARD;
    v.extreme_bg_color = Color32::from_rgb(0x12, 0x12, 0x12);

    // Widgets
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);

    v.widgets.inactive.bg_fill = CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);

    v.widgets.hovered.bg_fill = HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);  // hover 邊框亮
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);

    v.widgets.active.bg_fill = Color32::from_rgb(0x33, 0x33, 0x33);
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, VALUE);

    // Selection
    v.selection.bg_fill = ACCENT.linear_multiply(0.15);
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    // Shape
    v.window_corner_radius = 4.0.into();
    v.menu_corner_radius = 4.0.into();
    v.popup_shadow = egui::epaint::Shadow { offset: [1, 2], blur: 6, spread: 0, color: Color32::from_black_alpha(80) };

    ctx.set_visuals(v);

    // Spacing
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(SP_SM, SP_XS);
    style.spacing.button_padding = egui::vec2(SP_SM, SP_XS);
    ctx.set_style(style);
}

// ── Widgets ──────────────────────────────────────────────────────────────────

/// Card — dark bg + border + rounded 4px (spec: Rounding(4.0) + Stroke(1.0, #333))
pub fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(4.0)
        .stroke(Stroke::new(1.0, BORDER))
        .inner_margin(SP_MD)
        .show(ui, |ui| content(ui));
    ui.add_space(SP_SM);
}

/// Section header (dim label) + indented content.
pub fn section(ui: &mut egui::Ui, title: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(SP_SM);
    ui.label(RichText::new(title).size(FONT_SMALL).strong().color(TEXT_DIM));
    ui.add_space(SP_XS);
    ui.indent(title, |ui| { content(ui); });
    ui.add_space(SP_SM);
}

/// Status dot (filled circle) + text. ACCENT=running, DANGER=stopped.
pub fn status_dot(ui: &mut egui::Ui, label: &str, ok: bool) {
    ui.horizontal(|ui| {
        let color = if ok { ACCENT } else { DANGER };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, color);
        ui.label(RichText::new(label).size(FONT_BODY).color(TEXT));
    });
}

/// Status row: label left, dot+status right.
pub fn status_row(ui: &mut egui::Ui, label: &str, status: &str, ok: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(FONT_BODY).color(TEXT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let color = if ok { ACCENT } else { DANGER };
            ui.colored_label(color, RichText::new(format!("● {status}")).size(FONT_SMALL));
        });
    });
}

/// Pill badge.
pub fn badge(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.linear_multiply(0.12))
        .corner_radius(3.0)
        .stroke(Stroke::new(0.5, color.linear_multiply(0.3)))
        .inner_margin(egui::vec2(6.0, 1.0))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(FONT_CAPTION).color(color));
        });
}

/// Count badge (accent bg).
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

/// Underline tab bar.
pub fn tab_bar(ui: &mut egui::Ui, labels: &[&str], selected: &mut usize) {
    ui.horizontal(|ui| {
        for (i, label) in labels.iter().enumerate() {
            let active = *selected == i;
            let color = if active { ACCENT } else { TEXT_DIM };
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
    let r = ui.max_rect();
    ui.painter().line_segment(
        [egui::pos2(r.left(), ui.cursor().top()), egui::pos2(r.right(), ui.cursor().top())],
        Stroke::new(1.0, BORDER),
    );
    ui.add_space(SP_SM);
}

/// Key-value info row.
pub fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.colored_label(TEXT_DIM, RichText::new(label).size(FONT_SMALL));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).size(FONT_SMALL).color(TEXT).family(FontFamily::Monospace));
        });
    });
}

/// Map task status to colour.
pub fn status_color(status: &str) -> Color32 {
    match status {
        "DONE" => ACCENT,
        "PENDING" | "RUNNING" => YELLOW,
        "FOLLOWING" => INFO,
        "FOLLOWUP_NEEDED" => YELLOW,
        "FAILED" | "ERROR" => DANGER,
        _ => TEXT_DIM,
    }
}

/// Map log level to colour.
pub fn log_color(level: crate::ui_service::LogLevel) -> Color32 {
    use crate::ui_service::LogLevel;
    match level {
        LogLevel::Error => DANGER,
        LogLevel::Warn => YELLOW,
        LogLevel::Telegram => INFO,
        LogLevel::Research => ACCENT,
        LogLevel::Followup => YELLOW,
        LogLevel::Coding => Color32::from_rgb(0xBB, 0x86, 0xFC), // purple
        LogLevel::Teams => Color32::from_rgb(0x00, 0xCC, 0xCC),  // teal
        LogLevel::Info | LogLevel::Normal => TEXT_DIM,
    }
}
