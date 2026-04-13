//! Workspace — overview (tasks + memory search) + thinking stream + pending approvals.

use std::sync::Arc;
use eframe::egui::{self, Color32, RichText, ScrollArea};
use crate::ui_service::*;

#[derive(Default)]
pub struct WorkspaceState {
    tab: usize, // 0=overview, 1=thinking, 2=pending
    pending_cache: Vec<PendingReplyView>,
    pending_loaded_for: String,
    mem_query: String,
    mem_results: Vec<String>,
}

pub fn show(
    ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agents: &[AgentSummary],
    agent_idx: usize, tasks: &[TaskView],
    pending_counts: &std::collections::HashMap<String, usize>,
    state: &mut WorkspaceState,
) {
    let Some(agent) = agents.get(agent_idx) else { ui.label("Agent not found"); return; };
    let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);

    ui.heading(&agent.name);
    ui.colored_label(Color32::GRAY, format!("ID: {} | {}", agent.id, agent.platform));
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        if ui.selectable_label(state.tab == 0, "📊 概覽").clicked() { state.tab = 0; }
        if ui.selectable_label(state.tab == 1, "🧠 思考流").clicked() { state.tab = 1; }
        if ui.selectable_label(state.tab == 2, format!("📝 待確認 ({pending_n})")).clicked() { state.tab = 2; }
    });
    ui.separator();

    match state.tab {
        0 => show_overview(ui, svc, tasks, state),
        1 => show_thinking_stream(ui, tasks),
        2 => show_pending(ui, svc, &agent.id, state),
        _ => {}
    }
}

fn show_overview(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, tasks: &[TaskView], state: &mut WorkspaceState) {
    // Memory search
    ui.horizontal(|ui| {
        ui.label("🔍 記憶搜尋:");
        let resp = ui.text_edit_singleline(&mut state.mem_query);
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !state.mem_query.trim().is_empty() {
            state.mem_results = svc.search_memory(state.mem_query.trim(), 5);
        }
        if ui.button("搜尋").clicked() && !state.mem_query.trim().is_empty() {
            state.mem_results = svc.search_memory(state.mem_query.trim(), 5);
        }
    });

    if !state.mem_results.is_empty() {
        ui.add_space(4.0);
        egui::Frame::new().fill(Color32::from_rgb(20, 28, 40)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
            ui.label(RichText::new("搜尋結果").small().strong().color(Color32::GRAY));
            for result in &state.mem_results {
                ui.colored_label(Color32::from_rgb(160, 200, 255), RichText::new(result).small());
                ui.separator();
            }
        });
    }

    ui.add_space(8.0);
    ui.label(RichText::new("近期活動").strong().color(Color32::GRAY));
    ui.add_space(4.0);

    if tasks.is_empty() {
        ui.colored_label(Color32::DARK_GRAY, "目前沒有活動記錄");
        return;
    }

    ScrollArea::vertical().id_salt("tasks").show(ui, |ui| {
        for task in tasks.iter().take(30) {
            egui::Frame::new().fill(Color32::from_rgb(28, 32, 38)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&task.event).size(13.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let status = task.status.as_deref().unwrap_or("");
                        ui.colored_label(status_color(status), RichText::new(status).small());
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

fn show_thinking_stream(ui: &mut egui::Ui, tasks: &[TaskView]) {
    ui.label(RichText::new("Agent 執行追蹤").strong().color(Color32::GRAY));
    ui.add_space(4.0);

    // Show tasks that represent agent thinking/execution (not heartbeats)
    let thinking: Vec<&TaskView> = tasks.iter()
        .filter(|t| t.event.contains("adk") || t.event.contains("chat") || t.event.contains("research")
            || t.event.contains("coding") || t.event.contains("router") || t.event.contains("planner"))
        .take(50)
        .collect();

    if thinking.is_empty() {
        ui.colored_label(Color32::DARK_GRAY, "暫無 Agent 執行記錄");
        return;
    }

    ScrollArea::vertical().id_salt("thinking").show(ui, |ui| {
        for task in thinking {
            let icon = if task.event.contains("research") { "🔍" }
                else if task.event.contains("coding") { "💻" }
                else if task.event.contains("chat") { "💬" }
                else { "🧠" };

            egui::Frame::new().fill(Color32::from_rgb(24, 30, 44)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(icon);
                    ui.label(RichText::new(&task.event).size(13.0).color(Color32::from_rgb(120, 180, 255)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(Color32::DARK_GRAY, RichText::new(&task.timestamp).small());
                    });
                });
                if let Some(reason) = &task.reason {
                    ui.colored_label(Color32::from_rgb(160, 160, 160), RichText::new(reason).small());
                }
            });
            ui.add_space(2.0);
        }
    });
}

fn show_pending(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agent_id: &str, state: &mut WorkspaceState) {
    if state.pending_loaded_for != agent_id {
        state.pending_cache = svc.load_pending(agent_id);
        state.pending_loaded_for = agent_id.to_string();
    }

    if state.pending_cache.is_empty() {
        ui.colored_label(Color32::from_rgb(80, 200, 100), "✅ 沒有待確認的回覆");
        return;
    }

    ScrollArea::vertical().id_salt("pending").show(ui, |ui| {
        let mut action: Option<(String, bool)> = None;
        for reply in &state.pending_cache {
            egui::Frame::new().fill(Color32::from_rgb(28, 32, 38)).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::GRAY, "來自");
                    ui.colored_label(Color32::from_rgb(100, 180, 255), &reply.peer_name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(Color32::DARK_GRAY, RichText::new(&reply.created_at).small());
                    });
                });
                ui.add_space(4.0);
                egui::Frame::new().fill(Color32::from_rgb(20, 22, 26)).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
                    ui.label(RichText::new(&reply.original_message).size(13.0));
                });
                ui.add_space(4.0);
                egui::Frame::new().fill(Color32::from_rgb(20, 35, 55)).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
                    ui.colored_label(Color32::from_rgb(140, 190, 255), RichText::new(&reply.draft_reply).size(13.0));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("✓ 核准").color(Color32::WHITE)).fill(Color32::from_rgb(40, 120, 60))).clicked() {
                        action = Some((reply.id.clone(), true));
                    }
                    if ui.add(egui::Button::new(RichText::new("✗ 拒絕").color(Color32::WHITE)).fill(Color32::from_rgb(120, 40, 40))).clicked() {
                        action = Some((reply.id.clone(), false));
                    }
                });
            });
            ui.add_space(4.0);
        }
        if let Some((id, approve)) = action {
            if approve { svc.approve_reply(agent_id, &id); } else { svc.reject_reply(agent_id, &id); }
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
