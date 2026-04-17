//! Live screenshot display panel.

use std::sync::Arc;
use eframe::egui::{self, RichText, TextureOptions};
use crate::monitor::control::ControlState;
use crate::monitor::state::MonitorState;
use super::MonitorViewState;
use crate::ui_egui::theme;

pub fn show(
    ui: &mut egui::Ui,
    ctrl: &Arc<ControlState>,
    monitor_state: &Option<Arc<MonitorState>>,
    view_state: &mut MonitorViewState,
) {
    theme::card(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("SCREENSHOT")
                    .size(theme::FONT_SMALL)
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::SP_SM);

            let Some(ms) = monitor_state else {
                ui.colored_label(theme::TEXT_DIM, "Monitor not initialized");
                return;
            };

            // Refresh texture if new screenshot available
            if let Some((ts, jpeg_bytes)) = ms.latest_screenshot() {
                let is_new = view_state
                    .last_screenshot_ts
                    .map_or(true, |prev| prev < ts);
                if is_new {
                    if let Some(color_img) = decode_jpeg(&jpeg_bytes) {
                        let tex = ui.ctx().load_texture(
                            "monitor_screenshot",
                            color_img,
                            TextureOptions::LINEAR,
                        );
                        view_state.screenshot_tex = Some(tex);
                        view_state.last_screenshot_ts = Some(ts);
                    }
                }
            }

            if let Some(tex) = &view_state.screenshot_tex {
                let max_w = ui.available_width();
                let [tw, th] = tex.size();
                let aspect = th as f32 / tw.max(1) as f32;
                let w = max_w.min(tw as f32);
                let h = w * aspect;
                ui.image((tex.id(), egui::vec2(w, h)));
            } else {
                // Placeholder
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 160.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(rect, 4.0, theme::CARD);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    if ctrl.snapshot().paused {
                        "Stream paused"
                    } else {
                        "Waiting for screenshot\u{2026}"
                    },
                    egui::FontId::proportional(theme::FONT_SMALL),
                    theme::TEXT_DIM,
                );
            }

            ui.add_space(theme::SP_SM);

            // Pause stream toggle
            let paused_stream = ms.paused_stream();
            let stream_label = if paused_stream {
                "\u{25b6} Resume stream"
            } else {
                "\u{23f8} Pause stream"
            };
            if ui
                .small_button(RichText::new(stream_label).size(theme::FONT_SMALL))
                .clicked()
            {
                ms.set_paused_stream(!paused_stream);
            }
        });
    });
}

/// Decode JPEG bytes into `egui::ColorImage`. Returns `None` on any error.
fn decode_jpeg(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}
