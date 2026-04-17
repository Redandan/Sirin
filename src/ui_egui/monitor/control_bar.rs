//! Pause / Step / Abort control panel.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use eframe::egui::{self, RichText};
use crate::monitor::control::ControlState;
use crate::ui_egui::theme;

pub fn show(ui: &mut egui::Ui, ctrl: &Arc<ControlState>) {
    theme::card(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("CONTROL")
                    .size(theme::FONT_SMALL)
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::SP_SM);

            let snap = ctrl.snapshot();

            ui.horizontal(|ui| {
                // Pause / Resume
                if snap.paused {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("\u{25b6} Resume").color(theme::BG))
                                .fill(theme::ACCENT)
                                .corner_radius(4.0),
                        )
                        .clicked()
                    {
                        ctrl.paused.store(false, Relaxed);
                        ctrl.step.store(false, Relaxed);
                    }
                } else if ui
                    .add(
                        egui::Button::new(RichText::new("\u{23f8} Pause"))
                            .fill(theme::CARD)
                            .corner_radius(4.0),
                    )
                    .clicked()
                {
                    ctrl.paused.store(true, Relaxed);
                }

                // Step (only while paused and not aborted)
                if snap.paused && !snap.aborted {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("\u{23ed} Step"))
                                .fill(theme::CARD)
                                .corner_radius(4.0),
                        )
                        .clicked()
                    {
                        ctrl.step.store(true, Relaxed);
                        ctrl.paused.store(false, Relaxed);
                    }
                }

                // Abort / Reset
                if !snap.aborted {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("\u{23f9} Abort").color(theme::DANGER))
                                .fill(theme::CARD)
                                .corner_radius(4.0),
                        )
                        .clicked()
                    {
                        ctrl.aborted.store(true, Relaxed);
                        // Unblock any gate() that is waiting
                        ctrl.paused.store(false, Relaxed);
                    }
                } else if ui
                    .add(
                        egui::Button::new(RichText::new("\u{21ba} Reset").color(theme::ACCENT))
                            .fill(theme::CARD)
                            .corner_radius(4.0),
                    )
                    .clicked()
                {
                    ctrl.reset();
                }
            });

            ui.add_space(theme::SP_XS);

            // Status line
            let status = if snap.aborted {
                RichText::new("Session aborted")
                    .color(theme::DANGER)
                    .size(theme::FONT_SMALL)
            } else if snap.paused {
                RichText::new("Paused \u{2014} actions queued")
                    .color(theme::YELLOW)
                    .size(theme::FONT_SMALL)
            } else {
                RichText::new("Running normally")
                    .color(theme::ACCENT)
                    .size(theme::FONT_SMALL)
            };
            ui.label(status);
        });
    });
}
