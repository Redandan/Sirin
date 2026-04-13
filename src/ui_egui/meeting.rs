//! Meeting room — multi-agent conversation with backend integration.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::theme;
use crate::ui_service::*;

#[derive(Default)]
pub struct MeetingState {
    input: String,
    messages: Vec<(String, String)>,
    invited: std::collections::HashSet<String>,
    ai_loading: bool,
    ai_rx: Option<std::sync::mpsc::Receiver<(String, String)>>, // (agent_name, reply)
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agents: &[AgentSummary], state: &mut MeetingState) {
    ui.set_max_width(600.0);

    // Poll async AI reply
    if let Some(rx) = &state.ai_rx {
        if let Ok((name, reply)) = rx.try_recv() {
            state.messages.push((name.clone(), reply.clone()));
            svc.meeting_send(&name, &reply);
            state.ai_loading = false;
            state.ai_rx = None;
        }
    }

    let active = svc.meeting_active();

    if !active {
        // Invite screen
        theme::card(ui, |ui| {
            ui.label(RichText::new("開始新會議").strong().size(theme::FONT_HEADING).color(theme::TEXT));
            ui.add_space(theme::SP_MD);
            ui.label(RichText::new("選擇參與者:").color(theme::TEXT_DIM));
            ui.add_space(theme::SP_SM);

            for agent in agents.iter().filter(|a| a.enabled) {
                let mut checked = state.invited.contains(&agent.id);
                if ui.checkbox(&mut checked, RichText::new(&agent.name).color(theme::TEXT)).changed() {
                    if checked { state.invited.insert(agent.id.clone()); }
                    else { state.invited.remove(&agent.id); }
                }
            }

            ui.add_space(theme::SP_MD);
            let can_start = !state.invited.is_empty();
            if ui.add_enabled(can_start, egui::Button::new(RichText::new("🚀 開始會議").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked() {
                let participants: Vec<String> = state.invited.drain().collect();
                svc.meeting_start(participants);
                state.messages.clear();
            }
        });
        return;
    }

    // Active meeting
    let names: Vec<&str> = agents.iter().filter(|a| a.enabled).map(|a| a.name.as_str()).collect();
    ui.horizontal(|ui| {
        ui.colored_label(theme::TEXT_DIM, "參與者:");
        for name in &names { theme::badge(ui, name, theme::INFO); }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(RichText::new("結束會議").color(theme::BG)).fill(theme::DANGER).corner_radius(6.0)).clicked() {
                svc.meeting_end();
            }
        });
    });
    ui.separator();

    // Messages
    ScrollArea::vertical().id_salt("meeting").stick_to_bottom(true).auto_shrink(false)
        .max_height(ui.available_height() - 50.0).show(ui, |ui| {
            if state.messages.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(theme::SP_XL);
                    ui.label(RichText::new("💬").size(theme::SP_XL));
                    ui.colored_label(theme::TEXT_DIM, "輸入訊息開始對話");
                });
            }
            for (speaker, text) in &state.messages {
                let is_agent = speaker != "Operator";
                let name_color = if is_agent { theme::ACCENT } else { theme::INFO };
                theme::card(ui, |ui| {
                    ui.colored_label(name_color, RichText::new(speaker).size(theme::FONT_SMALL).strong());
                    ui.label(RichText::new(text).color(theme::TEXT));
                });
            }
            if state.ai_loading {
                ui.colored_label(theme::YELLOW, "● Agent 思考中...");
            }
        });

    // Input
    ui.separator();
    ui.add_space(theme::SP_SM);
    ui.horizontal(|ui| {
        let resp = ui.add_sized(
            [ui.available_width() - 60.0, 28.0],
            egui::TextEdit::singleline(&mut state.input).hint_text("輸入訊息..."),
        );
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let can_send = !state.input.trim().is_empty() && !state.ai_loading;
        if ui.add_enabled(can_send, egui::Button::new(RichText::new("發送").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked() || (enter && can_send)
        {
            let msg = state.input.trim().to_string();
            state.messages.push(("Operator".to_string(), msg.clone()));
            svc.meeting_send("Operator", &msg);
            state.input.clear();

            // Trigger AI agent reply in background
            let (tx, rx) = std::sync::mpsc::channel();
            let svc = svc.clone();
            let agent_name = agents.iter().find(|a| a.enabled).map(|a| a.name.clone()).unwrap_or("Agent".into());
            let agent_id = agents.iter().find(|a| a.enabled).map(|a| a.id.clone()).unwrap_or_default();
            std::thread::spawn(move || {
                let reply = svc.chat_send(&agent_id, &msg);
                let _ = tx.send((agent_name, reply));
            });
            state.ai_rx = Some(rx);
            state.ai_loading = true;
        }
    });
}
