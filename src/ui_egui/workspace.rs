//! Workspace — overview (tasks + memory search) + thinking stream + pending approvals.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::theme;
use crate::ui_service::*;

#[derive(Default)]
pub struct WorkspaceState {
    tab: usize,
    pending_cache: Vec<PendingReplyView>,
    pending_loaded_for: String,
    mem_query: String,
    mem_results: Vec<String>,
}

pub fn show(
    ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agents: &[AgentSummary],
    idx: usize, tasks: &[TaskView],
    pending_counts: &std::collections::HashMap<String, usize>, state: &mut WorkspaceState,
) {
    let Some(agent) = agents.get(idx) else { ui.label("Agent not found"); return; };
    let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);

    ui.label(RichText::new(&agent.name).heading().strong().color(theme::TEXT));
    ui.colored_label(theme::OVERLAY0, format!("ID: {} | {}", agent.id, agent.platform));
    ui.add_space(theme::GAP_MD);

    ui.horizontal(|ui| {
        if ui.selectable_label(state.tab == 0, RichText::new("📊 概覽").color(theme::TEXT)).clicked() { state.tab = 0; }
        if ui.selectable_label(state.tab == 1, RichText::new("🧠 思考流").color(theme::TEXT)).clicked() { state.tab = 1; }
        if ui.selectable_label(state.tab == 2, RichText::new(format!("📝 待確認 ({pending_n})")).color(theme::TEXT)).clicked() { state.tab = 2; }
    });
    ui.separator();

    match state.tab {
        0 => show_overview(ui, svc, tasks, state),
        1 => show_thinking(ui, tasks),
        2 => show_pending(ui, svc, &agent.id, state),
        _ => {}
    }
}

fn show_overview(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, tasks: &[TaskView], state: &mut WorkspaceState) {
    // Memory search
    ui.horizontal(|ui| {
        ui.label(RichText::new("🔍").color(theme::BLUE));
        let resp = ui.text_edit_singleline(&mut state.mem_query);
        if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) || ui.button("搜尋").clicked())
            && !state.mem_query.trim().is_empty() {
            state.mem_results = svc.search_memory(state.mem_query.trim(), 5);
        }
    });

    if !state.mem_results.is_empty() {
        ui.add_space(theme::GAP_SM);
        theme::card(ui, |ui| {
            ui.label(RichText::new("搜尋結果").small().strong().color(theme::OVERLAY0));
            for r in &state.mem_results {
                ui.colored_label(theme::LAVENDER, RichText::new(r).small());
                ui.separator();
            }
        });
    }

    ui.add_space(theme::GAP_MD);
    ui.label(RichText::new("近期活動").strong().color(theme::OVERLAY0));
    if tasks.is_empty() { ui.colored_label(theme::OVERLAY0, "目前沒有活動記錄"); return; }

    ScrollArea::vertical().id_salt("tasks").show(ui, |ui| {
        for task in tasks.iter().take(30) {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&task.event).size(13.0).color(theme::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let s = task.status.as_deref().unwrap_or("");
                        theme::badge(ui, s, theme::status_color(s));
                    });
                });
                if let Some(r) = &task.reason { ui.colored_label(theme::SUBTEXT0, RichText::new(r).small()); }
                ui.colored_label(theme::OVERLAY0, RichText::new(&task.timestamp).small());
            });
        }
    });
}

fn show_thinking(ui: &mut egui::Ui, tasks: &[TaskView]) {
    ui.label(RichText::new("Agent 執行追蹤").strong().color(theme::OVERLAY0));
    ui.add_space(theme::GAP_SM);

    let thinking: Vec<&TaskView> = tasks.iter()
        .filter(|t| ["adk", "chat", "research", "coding", "router", "planner"].iter().any(|k| t.event.contains(k)))
        .take(50).collect();

    if thinking.is_empty() { ui.colored_label(theme::OVERLAY0, "暫無執行記錄"); return; }

    ScrollArea::vertical().id_salt("thinking").show(ui, |ui| {
        for task in thinking {
            let icon = if task.event.contains("research") { "🔍" }
                else if task.event.contains("coding") { "💻" }
                else if task.event.contains("chat") { "💬" }
                else { "🧠" };
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(icon);
                    ui.label(RichText::new(&task.event).size(13.0).color(theme::BLUE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(theme::OVERLAY0, RichText::new(&task.timestamp).small());
                    });
                });
                if let Some(r) = &task.reason { ui.colored_label(theme::SUBTEXT0, RichText::new(r).small()); }
            });
        }
    });
}

fn show_pending(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agent_id: &str, state: &mut WorkspaceState) {
    if state.pending_loaded_for != agent_id {
        state.pending_cache = svc.load_pending(agent_id);
        state.pending_loaded_for = agent_id.to_string();
    }
    if state.pending_cache.is_empty() {
        theme::badge(ui, "✅ 沒有待確認的回覆", theme::GREEN);
        return;
    }

    ScrollArea::vertical().id_salt("pending").show(ui, |ui| {
        let mut action: Option<(String, bool)> = None;
        for reply in &state.pending_cache {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(theme::OVERLAY0, "來自");
                    ui.colored_label(theme::BLUE, &reply.peer_name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(theme::OVERLAY0, RichText::new(&reply.created_at).small());
                    });
                });
                ui.add_space(theme::GAP_SM);

                // Original message
                egui::Frame::new().fill(theme::CRUST).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                    ui.label(RichText::new(&reply.original_message).size(13.0).color(theme::SUBTEXT1));
                });
                ui.add_space(theme::GAP_SM);

                // Draft
                egui::Frame::new().fill(theme::BLUE.linear_multiply(0.08)).corner_radius(6.0).inner_margin(8.0)
                    .stroke(egui::Stroke::new(1.0, theme::BLUE.linear_multiply(0.2)))
                    .show(ui, |ui| {
                        ui.colored_label(theme::LAVENDER, RichText::new(&reply.draft_reply).size(13.0));
                    });
                ui.add_space(theme::GAP_MD);

                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("✓ 核准").color(theme::CRUST)).fill(theme::GREEN).corner_radius(6.0)).clicked() {
                        action = Some((reply.id.clone(), true));
                    }
                    if ui.add(egui::Button::new(RichText::new("✗ 拒絕").color(theme::CRUST)).fill(theme::RED).corner_radius(6.0)).clicked() {
                        action = Some((reply.id.clone(), false));
                    }
                });
            });
        }
        if let Some((id, approve)) = action {
            if approve { svc.approve_reply(agent_id, &id); } else { svc.reject_reply(agent_id, &id); }
            state.pending_cache = svc.load_pending(agent_id);
        }
    });
}
