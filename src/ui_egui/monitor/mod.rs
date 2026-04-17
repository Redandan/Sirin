//! Live Monitor view — real-time browser action feed + screenshot stream.

mod action_feed;
mod control_bar;
mod screenshot_pane;

use eframe::egui::{self, TextureHandle};

/// Persistent UI state for the Monitor view.
pub struct MonitorViewState {
    /// Cached screenshot texture. Refreshed when a new JPEG arrives.
    screenshot_tex: Option<TextureHandle>,
    /// Timestamp of the last screenshot we uploaded (to detect changes).
    last_screenshot_ts: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether the action feed is auto-scrolled to the bottom.
    auto_scroll: bool,
}

impl Default for MonitorViewState {
    fn default() -> Self {
        Self {
            screenshot_tex: None,
            last_screenshot_ts: None,
            auto_scroll: true,
        }
    }
}

/// Entry point called by `ui_egui/mod.rs`.
pub fn show(ui: &mut egui::Ui, state: &mut MonitorViewState) {
    // Mark view as active so screenshot pump runs
    if let Some(ms) = crate::monitor::state() {
        ms.set_view_active(true);
    }

    let ctrl = crate::monitor::control();
    let monitor_state = crate::monitor::state();

    // ── Status bar ──────────────────────────────────────────────────────
    crate::ui_egui::theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            // Active indicator
            let snap = ctrl.snapshot();
            let (color, label) = if snap.aborted {
                (crate::ui_egui::theme::DANGER, "ABORTED")
            } else if snap.paused {
                (crate::ui_egui::theme::YELLOW, "PAUSED")
            } else {
                (crate::ui_egui::theme::ACCENT, "ACTIVE")
            };
            ui.colored_label(
                color,
                egui::RichText::new(label)
                    .size(crate::ui_egui::theme::FONT_SMALL)
                    .strong(),
            );

            ui.add_space(crate::ui_egui::theme::SP_MD);

            // Connected clients
            if let Some(ms) = &monitor_state {
                let clients = ms.clients_snapshot();
                if !clients.is_empty() {
                    let mut sorted: Vec<_> = clients.into_iter().collect();
                    sorted.sort();
                    ui.label(
                        egui::RichText::new(format!("clients: {}", sorted.join(", ")))
                            .size(crate::ui_egui::theme::FONT_SMALL)
                            .color(crate::ui_egui::theme::TEXT_DIM),
                    );
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Clear button
                if let Some(ms) = &monitor_state {
                    if ui.small_button("Clear").clicked() {
                        ms.clear();
                        state.screenshot_tex = None;
                        state.last_screenshot_ts = None;
                    }
                }
                // Event count
                if let Some(ms) = &monitor_state {
                    ui.label(
                        egui::RichText::new(format!("{} events", ms.event_count()))
                            .size(crate::ui_egui::theme::FONT_SMALL)
                            .color(crate::ui_egui::theme::TEXT_DIM),
                    );
                }
            });
        });
    });

    ui.add_space(crate::ui_egui::theme::SP_SM);

    // ── Two-column layout: screenshot + control | action feed ───────────
    let available = ui.available_size();
    let left_w = (available.x * 0.42).max(280.0).min(480.0);

    ui.horizontal(|ui| {
        // Left column: screenshot + control bar
        ui.vertical(|ui| {
            ui.set_max_width(left_w);

            // Screenshot pane
            screenshot_pane::show(ui, &ctrl, &monitor_state, state);
            ui.add_space(crate::ui_egui::theme::SP_SM);

            // Control bar
            control_bar::show(ui, &ctrl);
        });

        ui.add_space(crate::ui_egui::theme::SP_SM);

        // Right column: action feed
        ui.vertical(|ui| {
            action_feed::show(ui, &monitor_state, &mut state.auto_scroll);
        });
    });
}
