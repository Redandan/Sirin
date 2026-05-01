//! Top status bar — 32pt header always visible above sidebar + central panel.
//!
//! Layout (left to right):
//!   [Sirin v0.4.7]  ●Browser  ●RPC  ●TG   ✓ PASS test_id   ───   url…  ⚙  ⌘K
//!
//! Browser/RPC/TG dots: green = running, red = stopped. Hover for tooltip.
//! Last verdict: pulled from recent_test_runs(1).
//! Browser URL: truncated to last 50 chars when long.
//! Gear → opens gear menu (commit 4 wires Settings/Logs/About).
//! ⌘K hint → palette opens via global shortcut (commit 3).

use std::sync::Arc;
use eframe::egui::{self, RichText};
use crate::ui_service::AppService;
use super::theme;

#[derive(Default, PartialEq, Clone, Copy)]
pub enum TopBarAction {
    #[default]
    None,
    OpenGearMenu,
    OpenPalette,
}

/// Render the 32pt top status bar. Returns the action triggered this frame.
pub fn show(ctx: &egui::Context, svc: &Arc<dyn AppService>) -> TopBarAction {
    let mut action = TopBarAction::None;

    egui::TopBottomPanel::top("sirin_top_bar")
        .exact_height(32.0)
        .frame(
            egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(egui::vec2(theme::SP_MD, 4.0))
                .stroke(egui::Stroke::new(0.5, theme::BORDER)),
        )
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                // ── Left cluster ──────────────────────────────────────
                ui.label(
                    RichText::new(concat!("Sirin v", env!("CARGO_PKG_VERSION")))
                        .size(theme::FONT_SMALL).strong().color(theme::TEXT),
                );
                ui.add_space(theme::SP_LG);

                let s = svc.system_status();
                draw_dot(ui, "Browser", svc.browser_is_open());
                draw_dot(ui, "RPC", s.rpc_running);
                draw_dot(ui, "TG", s.telegram_connected);

                ui.add_space(theme::SP_MD);

                // Last test verdict (newest run)
                let recent = svc.recent_test_runs(1);
                if let Some(last) = recent.first() {
                    let (col, txt) = match last.status.as_str() {
                        "passed"   => (theme::ACCENT, "✓ PASS"),
                        "failed"   => (theme::DANGER, "✗ FAIL"),
                        "timeout"  => (theme::YELLOW, "⌚ TIMEOUT"),
                        "error"    => (theme::DANGER, "✗ ERROR"),
                        "running"  => (theme::INFO,   "▶ RUNNING"),
                        "queued"   => (theme::TEXT_DIM, "⋯ QUEUED"),
                        other      => (theme::TEXT_DIM, other),
                    };
                    ui.colored_label(col,
                        RichText::new(txt).size(theme::FONT_CAPTION).strong());
                    ui.add_space(theme::SP_XS);
                    ui.colored_label(theme::TEXT_DIM,
                        RichText::new(&last.test_id)
                            .size(theme::FONT_CAPTION).monospace());
                }

                // ── Right cluster (right-to-left layout) ──────────────
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        // ⌘K hint — visual cue, also clickable as fallback
                        let resp = ui.add(
                            egui::Button::new(
                                RichText::new("⌘K")
                                    .size(theme::FONT_CAPTION)
                                    .monospace()
                                    .color(theme::TEXT_DIM),
                            )
                            .frame(false),
                        );
                        if resp.clicked() { action = TopBarAction::OpenPalette; }
                        resp.on_hover_text("Command palette (Ctrl+K / Cmd+K)");

                        ui.add_space(theme::SP_SM);

                        // Gear button
                        let resp = ui.add(
                            egui::Button::new(
                                RichText::new("⚙")
                                    .size(15.0)
                                    .color(theme::TEXT_DIM),
                            )
                            .frame(false),
                        );
                        if resp.clicked() { action = TopBarAction::OpenGearMenu; }
                        resp.on_hover_text("Settings · Logs · About");

                        ui.add_space(theme::SP_MD);

                        // Browser URL (truncated tail)
                        if let Some(url) = svc.browser_url() {
                            let trunc = truncate_url(&url, 60);
                            let url_for_hover = url.clone();
                            ui.add(egui::Label::new(
                                RichText::new(trunc)
                                    .size(theme::FONT_CAPTION)
                                    .monospace()
                                    .color(theme::TEXT_DIM),
                            ).truncate())
                            .on_hover_text(url_for_hover);
                        }
                    },
                );
            });
        });

    action
}

/// Single status dot + label. ~70px wide cell so layout stays predictable.
fn draw_dot(ui: &mut egui::Ui, label: &str, ok: bool) {
    let color = if ok { theme::ACCENT } else { theme::DANGER };
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(54.0, 16.0),
        egui::Sense::hover(),
    );
    let dot_pos = egui::pos2(rect.left() + 4.0, rect.center().y);
    ui.painter().circle_filled(dot_pos, 3.5, color);
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(theme::FONT_CAPTION),
        theme::TEXT_DIM,
    );
    resp.on_hover_text(format!(
        "{label}: {}",
        if ok { "running" } else { "stopped" }
    ));
}

/// Keep the tail of a URL within `max` chars (e.g. "…domain.com/path/x").
fn truncate_url(url: &str, max: usize) -> String {
    if url.chars().count() <= max {
        url.to_string()
    } else {
        let tail: String = url.chars().rev().take(max - 1).collect::<Vec<_>>()
            .into_iter().rev().collect();
        format!("…{tail}")
    }
}
