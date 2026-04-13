//! Left sidebar — three grouped sections + status bar.
//!
//! Layout (top to bottom):
//!   Logo + subtitle
//!   ─── AGENTS section ───
//!     Agent cards (name, status dot, pending badge, double-click rename)
//!   ─── TOOLS section ───
//!     ⚙ 設定 / 📋 Log
//!   ─── COLLAB section ───
//!     🔧 Workflow / 🤝 Meeting
//!   ─── STATUS bar (bottom-pinned) ───
//!     TG: ● connected / RPC: ● 7700

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::{View, theme};
use crate::ui_service::{AgentSummary, AppService};

pub fn show(
    ctx: &egui::Context, svc: &Arc<dyn AppService>, agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View, renaming: &mut Option<(usize, String)>,
) {
    egui::SidePanel::left("sidebar").resizable(false).exact_width(220.0)
        .frame(egui::Frame::new().fill(theme::MANTLE))
        .show(ctx, |ui| {
            ui.add_space(theme::GAP_LG);

            // ── Logo ─────────────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label(RichText::new("◆").size(20.0).color(theme::BLUE));
                ui.vertical(|ui| {
                    ui.label(RichText::new("Sirin").strong().size(16.0).color(theme::TEXT));
                    ui.label(RichText::new("AI Agent Platform").small().color(theme::OVERLAY0));
                });
            });
            ui.add_space(theme::GAP_MD);

            // ── AGENTS section ───────────────────────────────────────────────
            section_header(ui, "AGENTS");

            ScrollArea::vertical().id_salt("agents")
                .max_height(ui.available_height() - 220.0)
                .show(ui, |ui| {
                    let mut rename_commit: Option<(usize, String)> = None;

                    for (idx, agent) in agents.iter().enumerate() {
                        let is_selected = matches!(view, View::Workspace(i) if *i == idx);
                        let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
                        let is_renaming = renaming.as_ref().map(|(i, _)| *i == idx).unwrap_or(false);

                        let id = ui.id().with(("agent_card", idx));
                        let rect = ui.available_rect_before_wrap();
                        let response = ui.interact(rect, id, egui::Sense::click());
                        let bg = if is_selected { theme::SURFACE1 }
                            else if response.hovered() { theme::SURFACE0 }
                            else { egui::Color32::TRANSPARENT };

                        egui::Frame::new().fill(bg).corner_radius(6.0).inner_margin(egui::vec2(8.0, 6.0)).show(ui, |ui| {
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
                                    // Status dot
                                    let (dot, color) = match agent.live_status.as_str() {
                                        "connected" => ("●", theme::GREEN),
                                        "reconnecting" => ("◐", theme::YELLOW),
                                        "waiting" => ("◑", theme::PEACH),
                                        "error" => ("●", theme::RED),
                                        _ => if agent.enabled { ("○", theme::OVERLAY0) } else { ("○", theme::SURFACE2) },
                                    };
                                    ui.colored_label(color, dot);
                                    ui.label(RichText::new(&agent.name).strong().size(13.0).color(theme::TEXT));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        theme::count_badge(ui, pending_n);
                                    });
                                });
                                // Platform tag
                                ui.horizontal(|ui| {
                                    ui.add_space(16.0); // indent under dot
                                    let platform_color = match agent.platform.as_str() {
                                        "telegram" => theme::BLUE,
                                        "teams" => theme::TEAL,
                                        _ => theme::OVERLAY0,
                                    };
                                    ui.colored_label(platform_color, RichText::new(&agent.platform).small());
                                    ui.colored_label(theme::SURFACE2, RichText::new(&agent.id).small());
                                });
                            }
                        });
                        ui.add_space(1.0);
                    }

                    if let Some((idx, name)) = rename_commit {
                        if let Some(agent) = agents.get(idx) { svc.rename_agent(&agent.id, &name); }
                    }
                });

            ui.add_space(theme::GAP_SM);

            // ── TOOLS section ────────────────────────────────────────────────
            section_header(ui, "SYSTEM");
            nav_item(ui, "⚙", "系統設定", View::Settings, view);
            nav_item(ui, "📋", "系統 Log", View::Log, view);

            ui.add_space(theme::GAP_SM);

            // ── COLLAB section ───────────────────────────────────────────────
            section_header(ui, "COLLAB");
            nav_item(ui, "🔧", "Skill 開發", View::Workflow, view);
            nav_item(ui, "🤝", "多 Agent 會議", View::Meeting, view);

            // ── STATUS bar (bottom-pinned) ───────────────────────────────────
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(theme::GAP_SM);
                let status = svc.system_status();
                ui.horizontal(|ui| {
                    let (dot, color) = if status.rpc_running { ("●", theme::GREEN) } else { ("○", theme::RED) };
                    ui.colored_label(color, RichText::new(format!("{dot} RPC")).small());
                    let (dot2, color2) = if status.telegram_connected { ("●", theme::GREEN) } else { ("○", theme::RED) };
                    ui.colored_label(color2, RichText::new(format!("{dot2} TG")).small());
                });
                egui::Frame::new().fill(theme::SURFACE0.linear_multiply(0.5))
                    .corner_radius(0.0).inner_margin(egui::vec2(0.0, 0.5)).show(ui, |_| {});
            });
        });
}

/// Small uppercase section header.
fn section_header(ui: &mut egui::Ui, label: &str) {
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(RichText::new(label).small().strong().color(theme::SURFACE2));
    });
    ui.add_space(2.0);
}

/// A navigation item: icon + label, with active/hover states.
fn nav_item(ui: &mut egui::Ui, icon: &str, label: &str, target: View, current: &mut View) {
    let is_active = std::mem::discriminant(current) == std::mem::discriminant(&target);
    let text_color = if is_active { theme::TEXT } else { theme::SUBTEXT0 };
    let bg = if is_active { theme::SURFACE1 } else { egui::Color32::TRANSPARENT };

    let btn = egui::Button::new(
        RichText::new(format!("{icon}  {label}")).size(13.0).color(text_color)
    ).fill(bg).corner_radius(6.0);

    if ui.add_sized([ui.available_width(), 28.0], btn).clicked() {
        *current = target;
    }
}
