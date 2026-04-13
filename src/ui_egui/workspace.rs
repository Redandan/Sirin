//! Workspace — agent overview + pending approvals.
//!
//! AI reads this: top header with agent name, tab bar (Overview / Pending),
//! overview shows recent tasks list, pending shows approve/reject cards.

use std::sync::Arc;

use eframe::egui::{self, Color32, RichText, ScrollArea};

use crate::ui_service::*;

#[derive(Default)]
pub struct WorkspaceState {
    tab: usize, // 0=overview, 1=pending
    pending_cache: Vec<PendingReplyView>,
    pending_loaded_for: String,
}

pub fn show(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    agents: &[AgentSummary],
    agent_idx: usize,
    tasks: &[TaskView],
    pending_counts: &std::collections::HashMap<String, usize>,
    state: &mut WorkspaceState,
) {
    let Some(agent) = agents.get(agent_idx) else {
        ui.label("Agent not found");
        return;
    };
    let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);

    // Header
    ui.heading(&agent.name);
    ui.colored_label(Color32::GRAY, format!("ID: {}", agent.id));
    ui.add_space(4.0);

    // Tab bar
    ui.horizontal(|ui| {
        if ui.selectable_label(state.tab == 0, "📊 概覽").clicked() { state.tab = 0; }
        if ui.selectable_label(state.tab == 1, format!("📝 待確認 ({pending_n})")).clicked() {
            state.tab = 1;
        }
    });
    ui.separator();

    match state.tab {
        0 => show_overview(ui, tasks),
        1 => show_pending(ui, svc, &agent.id, state),
        _ => {}
    }
}

fn show_overview(ui: &mut egui::Ui, tasks: &[TaskView]) {
    ui.label(RichText::new("近期活動").strong().color(Color32::GRAY));
    ui.add_space(4.0);

    if tasks.is_empty() {
        ui.colored_label(Color32::DARK_GRAY, "目前沒有活動記錄");
        return;
    }

    ScrollArea::vertical().id_salt("tasks").show(ui, |ui| {
        for task in tasks.iter().take(30) {
            egui::Frame::new()
                .fill(Color32::from_rgb(28, 32, 38))
                .corner_radius(6.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&task.event).size(13.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let status = task.status.as_deref().unwrap_or("");
                            let color = status_color(status);
                            ui.colored_label(color, RichText::new(status).small());
                        });
                    });
                    if let Some(reason) = &task.reason {
                        ui.colored_label(Color32::GRAY, RichText::new(reason).small());
                    }
                    ui.colored_label(Color32::DARK_GRAY, RichText::new(&task.timestamp).small());
                });
            ui.add_space(2.0);
        }
    });
}

fn show_pending(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agent_id: &str, state: &mut WorkspaceState) {
    // Reload cache if agent changed
    if state.pending_loaded_for != agent_id {
        state.pending_cache = svc.load_pending(agent_id);
        state.pending_loaded_for = agent_id.to_string();
    }

    if state.pending_cache.is_empty() {
        ui.colored_label(Color32::from_rgb(80, 200, 100), "✅ 沒有待確認的回覆");
        return;
    }

    ScrollArea::vertical().id_salt("pending").show(ui, |ui| {
        let mut action: Option<(&str, bool)> = None; // (id, approve?)

        for reply in &state.pending_cache {
            egui::Frame::new()
                .fill(Color32::from_rgb(28, 32, 38))
                .corner_radius(6.0)
                .inner_margin(10.0)
                .show(ui, |ui| {
                    // From
                    ui.horizontal(|ui| {
                        ui.colored_label(Color32::GRAY, "來自");
                        ui.colored_label(Color32::from_rgb(100, 180, 255), &reply.peer_name);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.colored_label(Color32::DARK_GRAY, RichText::new(&reply.created_at).small());
                        });
                    });
                    ui.add_space(4.0);

                    // Original message
                    egui::Frame::new()
                        .fill(Color32::from_rgb(20, 22, 26))
                        .corner_radius(4.0)
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.label(RichText::new(&reply.original_message).size(13.0));
                        });
                    ui.add_space(4.0);

                    // Draft
                    egui::Frame::new()
                        .fill(Color32::from_rgb(20, 35, 55))
                        .corner_radius(4.0)
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.colored_label(Color32::from_rgb(140, 190, 255), RichText::new(&reply.draft_reply).size(13.0));
                        });
                    ui.add_space(6.0);

                    // Buttons
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("✓ 核准").color(Color32::WHITE))
                            .fill(Color32::from_rgb(40, 120, 60))).clicked() {
                            action = Some((&reply.id, true));
                        }
                        if ui.add(egui::Button::new(RichText::new("✗ 拒絕").color(Color32::WHITE))
                            .fill(Color32::from_rgb(120, 40, 40))).clicked() {
                            action = Some((&reply.id, false));
                        }
                    });
                });
            ui.add_space(4.0);
        }

        // Apply action after iteration
        if let Some((id, approve)) = action {
            if approve {
                svc.approve_reply(agent_id, id);
            } else {
                svc.reject_reply(agent_id, id);
            }
            state.pending_cache = svc.load_pending(agent_id);
        }
    });
}

fn status_color(status: &str) -> Color32 {
    match status {
        "DONE" => Color32::from_rgb(100, 220, 100),
        "PENDING" | "RUNNING" => Color32::from_rgb(255, 200, 60),
        "FOLLOWING" => Color32::from_rgb(120, 180, 255),
        "FOLLOWUP_NEEDED" => Color32::from_rgb(255, 160, 80),
        "FAILED" | "ERROR" => Color32::from_rgb(220, 80, 80),
        _ => Color32::GRAY,
    }
}
