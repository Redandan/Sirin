//! Sidebar — Claude Desktop style: pure text list, selected = left accent bar.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::{View, theme};
use crate::ui_service::{AgentSummary, AppService};

pub fn show(
    ctx: &egui::Context, svc: &Arc<dyn AppService>, agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View, renaming: &mut Option<(usize, String)>,
) {
    egui::SidePanel::left("sidebar").resizable(false).exact_width(200.0)
        .frame(egui::Frame::new().fill(theme::BG))
        .show(ctx, |ui| {
            ui.add_space(theme::SP_LG);
            ui.add_space(theme::SP_SM);

            // ── Agent list ───────────────────────────────────────────────
            ScrollArea::vertical().id_salt("agents")
                .max_height(ui.available_height() - 180.0)
                .show(ui, |ui| {
                    let mut rename_commit: Option<(usize, String)> = None;

                    for (idx, agent) in agents.iter().enumerate() {
                        let is_selected = matches!(view, View::Workspace(i) if *i == idx);
                        let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
                        let is_renaming = renaming.as_ref().map(|(i, _)| *i == idx).unwrap_or(false);

                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 30.0),
                            egui::Sense::click(),
                        );

                        // Selected: left accent bar + dark bg
                        if is_selected {
                            ui.painter().rect_filled(rect, 4.0, theme::HOVER);
                            // Left accent bar (3px wide)
                            let bar = egui::Rect::from_min_size(
                                rect.left_top(),
                                egui::vec2(3.0, rect.height()),
                            );
                            ui.painter().rect_filled(bar, 2.0, theme::ACCENT);
                        } else if response.hovered() {
                            ui.painter().rect_filled(rect, 4.0, theme::CARD);
                        }

                        let content_rect = rect.shrink2(egui::vec2(theme::SP_MD, 0.0));

                        if is_renaming {
                            let buf = &mut renaming.as_mut().unwrap().1;
                            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(content_rect));
                            let resp = child.text_edit_singleline(buf);
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

                            let text_color = if is_selected { theme::TEXT } else { theme::TEXT_DIM };
                            let name_pos = egui::pos2(content_rect.left(), content_rect.center().y - 7.0);
                            ui.painter().text(
                                name_pos, egui::Align2::LEFT_TOP, &agent.name,
                                egui::FontId::proportional(theme::FONT_BODY), text_color,
                            );

                            // Pending count (right side, small)
                            if pending_n > 0 {
                                let count_pos = egui::pos2(content_rect.right() - 4.0, content_rect.center().y);
                                ui.painter().text(
                                    count_pos, egui::Align2::RIGHT_CENTER,
                                    &format!("{pending_n}"),
                                    egui::FontId::proportional(theme::FONT_CAPTION),
                                    theme::ACCENT,
                                );
                            }
                        }
                    }

                    if let Some((idx, name)) = rename_commit {
                        if let Some(agent) = agents.get(idx) { svc.rename_agent(&agent.id, &name); }
                    }
                });

            ui.add_space(theme::SP_SM);

            // ── Section: dimmed label ────────────────────────────────────
            sidebar_label(ui, "System");
            sidebar_item(ui, "系統設定", View::Settings, view);
            sidebar_item(ui, "Log", View::Log, view);

            ui.add_space(theme::SP_SM);
            sidebar_label(ui, "Develop");
            sidebar_item(ui, "Skill 開發", View::Workflow, view);
            sidebar_item(ui, "會議室", View::Meeting, view);

            // ── Bottom status ────────────────────────────────────────────
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(theme::SP_SM);
                let status = svc.system_status();
                ui.horizontal(|ui| {
                    ui.add_space(theme::SP_MD);
                    let tg_color = if status.telegram_connected { theme::ACCENT } else { theme::DANGER };
                    let rpc_color = if status.rpc_running { theme::ACCENT } else { theme::DANGER };
                    ui.colored_label(tg_color, RichText::new("●").size(theme::FONT_CAPTION));
                    ui.colored_label(theme::TEXT_DIM, RichText::new("TG").size(theme::FONT_CAPTION));
                    ui.add_space(theme::SP_SM);
                    ui.colored_label(rpc_color, RichText::new("●").size(theme::FONT_CAPTION));
                    ui.colored_label(theme::TEXT_DIM, RichText::new("RPC").size(theme::FONT_CAPTION));
                });
            });
        });
}

fn sidebar_label(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(theme::SP_MD);
        ui.label(RichText::new(text).size(theme::FONT_SMALL).color(theme::TEXT_DIM));
    });
    ui.add_space(theme::SP_XS);
}

fn sidebar_item(ui: &mut egui::Ui, label: &str, target: View, current: &mut View) {
    let active = std::mem::discriminant(current) == std::mem::discriminant(&target);

    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 28.0),
        egui::Sense::click(),
    );

    if active {
        ui.painter().rect_filled(rect, 4.0, theme::HOVER);
        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height()));
        ui.painter().rect_filled(bar, 2.0, theme::ACCENT);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 4.0, theme::CARD);
    }

    let text_color = if active { theme::TEXT } else { theme::TEXT_DIM };
    let pos = egui::pos2(rect.left() + theme::SP_MD, rect.center().y - 6.5);
    ui.painter().text(pos, egui::Align2::LEFT_TOP, label, egui::FontId::proportional(theme::FONT_BODY), text_color);

    if response.clicked() { *current = target; }
}
