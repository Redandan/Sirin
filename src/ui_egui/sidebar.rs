//! Sidebar — Claude Desktop style with Sirin branding.
//!
//! Layout (top → bottom):
//!   "Sirin" title
//!   AGENTS label → agent items (dot + name + count)
//!   SYSTEM label → 系統設定, Log
//!   DEVELOP label → Skill 開發, 會議室
//!   ── separator ──
//!   TG ● RPC ●

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::{View, theme};
use crate::ui_service::{AgentSummary, AppService};

pub fn show(
    ctx: &egui::Context, svc: &Arc<dyn AppService>, agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View, renaming: &mut Option<(usize, String)>,
) {
    egui::SidePanel::left("sidebar").resizable(false).exact_width(210.0)
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::vec2(theme::SP_SM, 0.0)))
        .show(ctx, |ui| {
            ui.add_space(theme::SP_LG);

            // ── Title ────────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.add_space(theme::SP_MD);
                ui.label(RichText::new("Sirin").size(theme::FONT_TITLE).strong().color(theme::TEXT));
            });
            ui.add_space(theme::SP_LG);

            // ── AGENTS ───────────────────────────────────────────────────
            group_label(ui, "AGENTS");

            ScrollArea::vertical().id_salt("agents")
                .max_height(ui.available_height() - 200.0)
                .show(ui, |ui| {
                    let mut rename_commit: Option<(usize, String)> = None;

                    for (idx, agent) in agents.iter().enumerate() {
                        let is_selected = matches!(view, View::Workspace(i) if *i == idx);
                        let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
                        let is_renaming = renaming.as_ref().map(|(i, _)| *i == idx).unwrap_or(false);

                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 28.0),
                            egui::Sense::click(),
                        );

                        // Background + left accent bar (inside rect, not offset)
                        if is_selected {
                            ui.painter().rect_filled(rect, 4.0, theme::HOVER);
                            let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height()));
                            ui.painter().rect_filled(bar, 1.0, theme::ACCENT);
                        } else if response.hovered() {
                            ui.painter().rect_filled(rect, 4.0, theme::CARD);
                        }

                        // Content area with left padding for bar + dot
                        let inner = rect.shrink2(egui::vec2(theme::SP_SM, 0.0));

                        if is_renaming {
                            let buf = &mut renaming.as_mut().unwrap().1;
                            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
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
                            response.on_hover_text(format!("{} ({}) — {}", agent.name, agent.platform, agent.live_status));

                            // Status dot
                            let dot_color = match agent.live_status.as_str() {
                                "connected" => theme::ACCENT,
                                "reconnecting" | "waiting" => theme::YELLOW,
                                "error" => theme::DANGER,
                                _ => if agent.enabled { theme::TEXT_DIM } else { theme::BORDER },
                            };
                            let dot_center = egui::pos2(inner.left() + 8.0, inner.center().y);
                            ui.painter().circle_filled(dot_center, 3.0, dot_color);

                            // Name
                            let text_color = if is_selected { theme::TEXT } else { theme::TEXT_DIM };
                            let name_pos = egui::pos2(inner.left() + 20.0, inner.center().y - 6.5);
                            ui.painter().text(
                                name_pos, egui::Align2::LEFT_TOP, &agent.name,
                                egui::FontId::proportional(theme::FONT_BODY), text_color,
                            );

                            // Pending count
                            if pending_n > 0 {
                                let pos = egui::pos2(inner.right() - 4.0, inner.center().y);
                                ui.painter().text(
                                    pos, egui::Align2::RIGHT_CENTER, format!("{pending_n}"),
                                    egui::FontId::proportional(theme::FONT_CAPTION), theme::ACCENT,
                                );
                            }
                        }
                    }

                    if let Some((idx, name)) = rename_commit {
                        if let Some(agent) = agents.get(idx) { svc.rename_agent(&agent.id, &name); }
                    }
                });

            ui.add_space(theme::SP_SM);
            theme::thin_separator(ui);

            // ── SYSTEM ───────────────────────────────────────────────────
            group_label(ui, "SYSTEM");
            nav_item(ui, "系統設定", View::Settings, view);
            nav_item(ui, "Log", View::Log, view);

            ui.add_space(theme::SP_XS);
            theme::thin_separator(ui);

            // ── DEVELOP ──────────────────────────────────────────────────
            group_label(ui, "DEVELOP");
            nav_item(ui, "Skill 開發", View::Workflow, view);
            nav_item(ui, "會議室", View::Meeting, view);

            // ── Bottom status ────────────────────────────────────────────
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(theme::SP_SM);
                let status = svc.system_status();
                ui.horizontal(|ui| {
                    ui.add_space(theme::SP_MD);
                    status_indicator(ui, "TG", status.telegram_connected);
                    ui.add_space(theme::SP_MD);
                    status_indicator(ui, "RPC", status.rpc_running);
                });
                ui.add_space(theme::SP_XS);
                theme::thin_separator(ui);
            });
        });
}

fn group_label(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(theme::SP_MD);
        ui.label(RichText::new(text).size(theme::FONT_CAPTION).strong().color(theme::TEXT_DIM));
    });
    ui.add_space(theme::SP_XS);
}

fn nav_item(ui: &mut egui::Ui, label: &str, target: View, current: &mut View) {
    let active = std::mem::discriminant(current) == std::mem::discriminant(&target);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 28.0),
        egui::Sense::click(),
    );

    if active {
        ui.painter().rect_filled(rect, 4.0, theme::HOVER);
        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height()));
        ui.painter().rect_filled(bar, 1.0, theme::ACCENT);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 4.0, theme::CARD);
    }

    let text_color = if active { theme::TEXT } else { theme::TEXT_DIM };
    let pos = egui::pos2(rect.left() + theme::SP_MD, rect.center().y - 6.5);
    ui.painter().text(pos, egui::Align2::LEFT_TOP, label, egui::FontId::proportional(theme::FONT_BODY), text_color);

    if response.clicked() { *current = target; }
}

fn status_indicator(ui: &mut egui::Ui, label: &str, ok: bool) {
    let color = if ok { theme::ACCENT } else { theme::DANGER };
    ui.colored_label(color, RichText::new("●").size(theme::FONT_CAPTION));
    ui.colored_label(theme::TEXT_DIM, RichText::new(label).size(theme::FONT_CAPTION));
}
