//! Left sidebar — Slack-inspired: logo, agent list (name + dot only), grouped nav, status bar.

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
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::vec2(theme::SP_SM, 0.0))
            .stroke(egui::Stroke::new(1.0, theme::BORDER)))
        .show(ctx, |ui| {
            ui.add_space(theme::SP_LG);

            // ── Logo ─────────────────────────────────────────────────────
            ui.label(RichText::new("Sirin").size(theme::FONT_HEADING).strong().color(theme::TEXT));
            ui.add_space(theme::SP_XL);

            // ── AGENTS ───────────────────────────────────────────────────
            section_label(ui, "AGENTS");

            ScrollArea::vertical().id_salt("agents")
                .max_height(ui.available_height() - 200.0)
                .show(ui, |ui| {
                    let mut rename_commit: Option<(usize, String)> = None;

                    for (idx, agent) in agents.iter().enumerate() {
                        let is_selected = matches!(view, View::Workspace(i) if *i == idx);
                        let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
                        let is_renaming = renaming.as_ref().map(|(i, _)| *i == idx).unwrap_or(false);

                        // Interaction
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 32.0),
                            egui::Sense::click(),
                        );

                        // Background: selected or hovered
                        if is_selected {
                            ui.painter().rect_filled(rect, 6.0, theme::HOVER);
                        } else if response.hovered() {
                            ui.painter().rect_filled(rect, 6.0, theme::CARD);
                        }

                        // Content
                        let content_rect = rect.shrink2(egui::vec2(theme::SP_SM, 0.0));
                        if is_renaming {
                            let buf = &mut renaming.as_mut().unwrap().1;
                            let mut r = ui.child_ui(content_rect, *ui.layout(), None);
                            let resp = r.text_edit_singleline(buf);
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

                            // Draw: dot + name + (badge)
                            let dot_color = match agent.live_status.as_str() {
                                "connected" => theme::ACCENT,
                                "reconnecting" => theme::YELLOW,
                                "waiting" => theme::YELLOW,
                                "error" => theme::DANGER,
                                _ => if agent.enabled { theme::TEXT_DIM } else { theme::BORDER },
                            };

                            let text_color = if is_selected { theme::TEXT } else { theme::TEXT_DIM };

                            // Dot
                            let dot_center = egui::pos2(content_rect.left() + 6.0, content_rect.center().y);
                            ui.painter().circle_filled(dot_center, 4.0, dot_color);

                            // Name
                            let name_pos = egui::pos2(content_rect.left() + 18.0, content_rect.center().y - 7.0);
                            ui.painter().text(
                                name_pos, egui::Align2::LEFT_TOP,
                                &agent.name,
                                egui::FontId::proportional(theme::FONT_BODY),
                                text_color,
                            );

                            // Pending badge (right-aligned)
                            if pending_n > 0 {
                                let badge_text = format!("{pending_n}");
                                let badge_pos = egui::pos2(content_rect.right() - 8.0, content_rect.center().y);
                                let badge_rect = egui::Rect::from_center_size(badge_pos, egui::vec2(20.0, 16.0));
                                ui.painter().rect_filled(badge_rect, 8.0, theme::YELLOW);
                                ui.painter().text(
                                    badge_pos, egui::Align2::CENTER_CENTER,
                                    &badge_text,
                                    egui::FontId::proportional(theme::FONT_CAPTION),
                                    theme::BG,
                                );
                            }
                        }
                    }

                    if let Some((idx, name)) = rename_commit {
                        if let Some(agent) = agents.get(idx) { svc.rename_agent(&agent.id, &name); }
                    }
                });

            ui.add_space(theme::SP_MD);

            // ── SYSTEM ───────────────────────────────────────────────────
            section_label(ui, "SYSTEM");
            nav_item(ui, "⚙  系統設定", View::Settings, view);
            nav_item(ui, "📋  Log", View::Log, view);

            ui.add_space(theme::SP_SM);

            // ── DEVELOP ──────────────────────────────────────────────────
            section_label(ui, "DEVELOP");
            nav_item(ui, "🔧  Skill 開發", View::Workflow, view);
            nav_item(ui, "🤝  會議室", View::Meeting, view);

            // ── STATUS (bottom) ──────────────────────────────────────────
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(theme::SP_SM);
                let status = svc.system_status();
                ui.horizontal(|ui| {
                    status_dot(ui, "TG", status.telegram_connected);
                    ui.add_space(theme::SP_MD);
                    status_dot(ui, "RPC", status.rpc_running);
                });
                ui.add_space(theme::SP_XS);
                // Thin separator
                ui.painter().line_segment(
                    [ui.cursor().left_top(), egui::pos2(ui.cursor().left_top().x + ui.available_width(), ui.cursor().left_top().y)],
                    egui::Stroke::new(1.0, theme::CARD),
                );
            });
        });
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(theme::SP_XS);
    ui.label(RichText::new(text).size(theme::FONT_CAPTION).strong().color(theme::BORDER));
    ui.add_space(theme::SP_XS);
}

fn nav_item(ui: &mut egui::Ui, label: &str, target: View, current: &mut View) {
    let active = std::mem::discriminant(current) == std::mem::discriminant(&target);
    let text_color = if active { theme::TEXT } else { theme::TEXT_DIM };

    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 30.0),
        egui::Sense::click(),
    );

    if active {
        ui.painter().rect_filled(rect, 6.0, theme::CARD);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 6.0, theme::CARD.linear_multiply(0.5));
    }

    let text_pos = egui::pos2(rect.left() + theme::SP_SM, rect.center().y - 7.0);
    ui.painter().text(
        text_pos, egui::Align2::LEFT_TOP, label,
        egui::FontId::proportional(theme::FONT_BODY), text_color,
    );

    if response.clicked() { *current = target; }
}

fn status_dot(ui: &mut egui::Ui, label: &str, ok: bool) {
    let color = if ok { theme::ACCENT } else { theme::DANGER };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.0, color);
        ui.colored_label(theme::TEXT_DIM, RichText::new(label).size(theme::FONT_CAPTION));
    });
}
