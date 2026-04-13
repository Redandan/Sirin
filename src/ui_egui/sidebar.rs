//! Left sidebar — agent cards with hover/selected states, inline rename, nav buttons.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::{View, theme};
use crate::ui_service::{AgentSummary, AppService};

pub fn show(
    ctx: &egui::Context, svc: &Arc<dyn AppService>, agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View, renaming: &mut Option<(usize, String)>,
) {
    egui::SidePanel::left("sidebar").resizable(false).exact_width(215.0)
        .frame(egui::Frame::new().fill(theme::MANTLE))
        .show(ctx, |ui| {
            ui.add_space(theme::GAP_LG);
            ui.label(RichText::new("Sirin").heading().strong().color(theme::TEXT));
            ui.label(RichText::new("AI Agent Platform").small().color(theme::OVERLAY0));
            ui.add_space(theme::GAP_SM);
            ui.separator();

            ScrollArea::vertical().id_salt("agents").max_height(ui.available_height() - 120.0).show(ui, |ui| {
                let mut rename_commit: Option<(usize, String)> = None;

                for (idx, agent) in agents.iter().enumerate() {
                    let is_selected = matches!(view, View::Workspace(i) if *i == idx);
                    let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
                    let is_renaming = renaming.as_ref().map(|(i, _)| *i == idx).unwrap_or(false);

                    // Determine background: selected > hovered > transparent
                    let id = ui.id().with(("agent_card", idx));
                    let rect = ui.available_rect_before_wrap();
                    let response = ui.interact(rect, id, egui::Sense::click());
                    let bg = if is_selected { theme::SURFACE1 }
                        else if response.hovered() { theme::SURFACE0 }
                        else { egui::Color32::TRANSPARENT };

                    egui::Frame::new().fill(bg).corner_radius(theme::CARD_RADIUS).inner_margin(theme::GAP_MD).show(ui, |ui| {
                        if is_renaming {
                            let buf = &mut renaming.as_mut().unwrap().1;
                            let resp = ui.text_edit_singleline(buf);
                            if resp.lost_focus() {
                                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                    rename_commit = Some((idx, buf.clone()));
                                }
                                *renaming = None;
                            }
                            resp.request_focus();
                        } else {
                            if response.clicked() { *view = View::Workspace(idx); }
                            if response.double_clicked() { *renaming = Some((idx, agent.name.clone())); }

                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&agent.name).strong().size(14.0).color(theme::TEXT));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    theme::count_badge(ui, pending_n);
                                });
                            });
                            ui.horizontal(|ui| {
                                let (dot, color) = match agent.live_status.as_str() {
                                    "connected" => ("●", theme::GREEN),
                                    "reconnecting" => ("◐", theme::YELLOW),
                                    "waiting" => ("◑", theme::PEACH),
                                    "error" => ("●", theme::RED),
                                    _ => if agent.enabled { ("○", theme::OVERLAY0) } else { ("○", theme::SURFACE2) },
                                };
                                ui.colored_label(color, RichText::new(dot).small());
                                ui.colored_label(theme::OVERLAY0, RichText::new(&agent.id).small());
                                if agent.live_status != "idle" {
                                    ui.colored_label(color, RichText::new(&agent.live_status).small());
                                }
                            });
                        }
                    });
                    ui.add_space(2.0);
                }

                if let Some((idx, name)) = rename_commit {
                    if let Some(agent) = agents.get(idx) { svc.rename_agent(&agent.id, &name); }
                }
            });

            ui.separator();

            // Nav buttons with hover effect
            for (label, target) in [
                ("⚙ 設定", View::Settings), ("📋 Log", View::Log),
                ("🔧 Workflow", View::Workflow), ("🤝 Meeting", View::Meeting),
            ] {
                let active = std::mem::discriminant(view) == std::mem::discriminant(&target);
                let btn = egui::Button::new(RichText::new(label).size(13.0).color(if active { theme::TEXT } else { theme::SUBTEXT0 }))
                    .fill(if active { theme::SURFACE1 } else { egui::Color32::TRANSPARENT })
                    .corner_radius(6.0);
                if ui.add_sized([ui.available_width(), 30.0], btn).clicked() { *view = target; }
            }
        });
}
