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
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agents: &[AgentSummary], state: &mut MeetingState) {
    let active = svc.meeting_active();

    if !active {
        // Invite screen
        theme::card(ui, |ui| {
            ui.label(RichText::new("開始新會議").strong().size(theme::FONT_HEADING).color(theme::TEXT));
            ui.add_space(theme::SP_MD);
            ui.label(RichText::new("選擇參與者:").color(theme::SUBTEXT0));
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
            if ui.add_enabled(can_start, egui::Button::new(RichText::new("🚀 開始會議").color(theme::CRUST)).fill(theme::BLUE).corner_radius(6.0)).clicked() {
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
        ui.colored_label(theme::OVERLAY0, "參與者:");
        for name in &names { theme::badge(ui, name, theme::BLUE); }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(RichText::new("結束會議").color(theme::CRUST)).fill(theme::RED).corner_radius(6.0)).clicked() {
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
                    ui.label(RichText::new("💬").size(theme::SP_XL * 2.0));
                    ui.colored_label(theme::OVERLAY0, "輸入訊息開始對話");
                });
            }
            for (speaker, text) in &state.messages {
                theme::card(ui, |ui| {
                    ui.colored_label(theme::BLUE, RichText::new(speaker).size(theme::FONT_SMALL).strong());
                    ui.label(RichText::new(text).color(theme::TEXT));
                });
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
        if (ui.add(egui::Button::new(RichText::new("發送").color(theme::CRUST)).fill(theme::BLUE).corner_radius(6.0)).clicked() || enter)
            && !state.input.trim().is_empty()
        {
            let msg = state.input.trim().to_string();
            state.messages.push(("Operator".to_string(), msg.clone()));
            svc.meeting_send("Operator", &msg);
            state.input.clear();
        }
    });
}
