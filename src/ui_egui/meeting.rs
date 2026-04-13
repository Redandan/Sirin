//! Meeting room — multi-agent conversation with themed cards.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::theme;
use crate::ui_service::*;

#[derive(Default)]
pub struct MeetingState {
    input: String,
    messages: Vec<(String, String)>,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agents: &[AgentSummary], state: &mut MeetingState) {
    let names: Vec<&str> = agents.iter().filter(|a| a.enabled).map(|a| a.name.as_str()).collect();
    ui.horizontal(|ui| {
        ui.colored_label(theme::OVERLAY0, "參與者:");
        for name in &names { theme::badge(ui, name, theme::BLUE); }
    });
    ui.separator();

    // Messages
    ScrollArea::vertical().id_salt("meeting").stick_to_bottom(true).auto_shrink(false)
        .max_height(ui.available_height() - 50.0).show(ui, |ui| {
            if state.messages.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label(RichText::new("💬").size(32.0));
                    ui.colored_label(theme::OVERLAY0, "輸入訊息開始會議");
                });
            }
            for (speaker, text) in &state.messages {
                theme::card(ui, |ui| {
                    ui.colored_label(theme::BLUE, RichText::new(speaker).small().strong());
                    ui.label(RichText::new(text).color(theme::TEXT));
                });
            }
        });

    // Input
    ui.separator();
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
