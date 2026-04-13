//! Left sidebar — agent list (with inline rename) + navigation.

use std::sync::Arc;
use eframe::egui::{self, Color32, RichText, ScrollArea};
use super::View;
use crate::ui_service::{AgentSummary, AppService};

/// Persistent sidebar state.
static mut RENAME_STATE: Option<(usize, String)> = None;

pub fn show(
    ctx: &egui::Context,
    svc: &Arc<dyn AppService>,
    agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View,
) {
    egui::SidePanel::left("sidebar").resizable(false).exact_width(215.0).show(ctx, |ui| {
        ui.add_space(6.0);
        ui.label(RichText::new("Sirin").heading().strong());
        ui.label(RichText::new("AI Agent Platform").small().color(Color32::GRAY));
        ui.add_space(4.0);
        ui.separator();

        // Agent list
        ScrollArea::vertical().id_salt("sidebar_agents").max_height(ui.available_height() - 100.0).show(ui, |ui| {
            let mut rename_commit: Option<(usize, String)> = None;

            for (idx, agent) in agents.iter().enumerate() {
                let is_selected = matches!(view, View::Workspace(i) if *i == idx);
                let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
                let bg = if is_selected { Color32::from_rgb(30, 55, 90) } else { Color32::TRANSPARENT };

                // Check if this agent is being renamed
                let renaming = unsafe { RENAME_STATE.as_ref().map(|(i, _)| *i == idx).unwrap_or(false) };

                egui::Frame::new().fill(bg).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                    if renaming {
                        // Rename input
                        let buf = unsafe { &mut RENAME_STATE.as_mut().unwrap().1 };
                        let response = ui.text_edit_singleline(buf);
                        if response.lost_focus() {
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                rename_commit = Some((idx, buf.clone()));
                            }
                            unsafe { RENAME_STATE = None; }
                        }
                        response.request_focus();
                    } else {
                        // Normal display
                        let response = ui.interact(ui.max_rect(), ui.id().with(&agent.id), egui::Sense::click());
                        if response.clicked() {
                            *view = View::Workspace(idx);
                        }
                        if response.double_clicked() {
                            unsafe { RENAME_STATE = Some((idx, agent.name.clone())); }
                        }

                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&agent.name).strong().size(14.0));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if pending_n > 0 {
                                    egui::Frame::new().fill(Color32::from_rgb(210, 100, 20)).corner_radius(10.0)
                                        .inner_margin(egui::vec2(5.0, 1.0)).show(ui, |ui| {
                                            ui.label(RichText::new(format!("{pending_n}")).small().color(Color32::WHITE));
                                        });
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            let (dot, color) = if agent.enabled { ("●", Color32::from_rgb(80, 200, 100)) } else { ("○", Color32::GRAY) };
                            ui.colored_label(color, RichText::new(dot).small());
                            ui.colored_label(Color32::GRAY, RichText::new(&agent.id).small());
                        });
                    }
                });
                ui.add_space(2.0);
            }

            // Apply rename
            if let Some((idx, new_name)) = rename_commit {
                if let Some(agent) = agents.get(idx) {
                    svc.rename_agent(&agent.id, &new_name);
                }
            }
        });

        ui.separator();

        // Navigation
        for (label, target) in [
            ("⚙ 設定", View::Settings),
            ("📋 Log", View::Log),
            ("🔧 Workflow", View::Workflow),
            ("🤝 Meeting", View::Meeting),
        ] {
            let active = std::mem::discriminant(view) == std::mem::discriminant(&target);
            let bg = if active { Color32::from_rgb(50, 50, 55) } else { Color32::TRANSPARENT };
            let btn = egui::Button::new(RichText::new(label).size(13.0)).fill(bg).corner_radius(4.0);
            if ui.add_sized([ui.available_width(), 28.0], btn).clicked() {
                *view = target;
            }
        }
    });
}
