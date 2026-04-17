//! Live Monitor view — real-time browser action feed + screenshot stream.

mod action_feed;
mod authz_modal;
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

    // ── Replay mode ──────────────────────────────────────────────────────────
    /// Whether we're showing live events or a loaded replay.
    pub replay_mode: bool,
    /// Events loaded from a trace file (empty = not loaded).
    pub replay_events: Vec<crate::monitor::events::ServerEvent>,
    /// Name of the loaded trace file (for display).
    pub replay_file_name: String,
    /// Whether the trace file picker dropdown is open.
    pub show_file_picker: bool,
    /// Cached list of available trace files (refreshed on picker open).
    pub trace_files: Vec<std::path::PathBuf>,
}

impl Default for MonitorViewState {
    fn default() -> Self {
        Self {
            screenshot_tex: None,
            last_screenshot_ts: None,
            auto_scroll: true,
            replay_mode: false,
            replay_events: Vec::new(),
            replay_file_name: String::new(),
            show_file_picker: false,
            trace_files: Vec::new(),
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

    // ── Replay mode bar ──────────────────────────────────────────────────
    crate::ui_egui::theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            if state.replay_mode {
                // In replay mode: show file name + "Back to Live" button
                ui.colored_label(
                    crate::ui_egui::theme::INFO,
                    egui::RichText::new("\u{25b6} REPLAY")
                        .size(crate::ui_egui::theme::FONT_SMALL)
                        .strong(),
                );
                ui.add_space(crate::ui_egui::theme::SP_SM);
                ui.label(
                    egui::RichText::new(&state.replay_file_name)
                        .size(crate::ui_egui::theme::FONT_SMALL)
                        .color(crate::ui_egui::theme::TEXT_DIM),
                );
                ui.add_space(crate::ui_egui::theme::SP_MD);
                if ui.small_button("\u{25c9} Live").clicked() {
                    state.replay_mode = false;
                    state.replay_events.clear();
                    state.replay_file_name.clear();
                }
            } else {
                // Live mode: show "Load trace" button
                ui.label(
                    egui::RichText::new("Trace:")
                        .size(crate::ui_egui::theme::FONT_SMALL)
                        .color(crate::ui_egui::theme::TEXT_DIM),
                );
                ui.add_space(crate::ui_egui::theme::SP_SM);
                if ui.small_button("\u{1f4c2} Load replay\u{2026}").clicked() {
                    state.show_file_picker = !state.show_file_picker;
                    if state.show_file_picker {
                        state.trace_files = crate::monitor::replay::list_trace_files();
                    }
                }
            }
        });

        // File picker dropdown
        if state.show_file_picker && !state.replay_mode {
            ui.add_space(crate::ui_egui::theme::SP_SM);
            if state.trace_files.is_empty() {
                ui.colored_label(
                    crate::ui_egui::theme::TEXT_DIM,
                    egui::RichText::new("No trace files found in .sirin/")
                        .size(crate::ui_egui::theme::FONT_SMALL),
                );
            } else {
                for path in state.trace_files.clone() {
                    let name = crate::monitor::replay::display_name(&path);
                    if ui
                        .selectable_label(
                            false,
                            egui::RichText::new(&name).size(crate::ui_egui::theme::FONT_SMALL),
                        )
                        .clicked()
                    {
                        let events = crate::monitor::replay::load_trace(&path);
                        state.replay_events = events;
                        state.replay_file_name = name;
                        state.replay_mode = true;
                        state.show_file_picker = false;
                        state.auto_scroll = false; // replay starts at top
                    }
                }
            }
        }
    });

    ui.add_space(crate::ui_egui::theme::SP_SM);

    // ── Authz asks panel (shown when pending decisions exist) ────────────
    if authz_modal::show(ui) {
        ui.add_space(crate::ui_egui::theme::SP_SM);
    }

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
            let events: Vec<crate::monitor::events::ServerEvent> = if state.replay_mode {
                state.replay_events.clone()
            } else {
                monitor_state
                    .as_ref()
                    .map(|ms| ms.events_snapshot())
                    .unwrap_or_default()
            };
            action_feed::show(ui, &events, &mut state.auto_scroll);
        });
    });
}
