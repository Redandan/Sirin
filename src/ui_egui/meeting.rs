//! Meeting room — multi-agent conversation.

use std::sync::Arc;
use eframe::egui::{self, Color32, RichText, ScrollArea};
use crate::ui_service::*;

#[derive(Default)]
pub struct MeetingState {
    input: String,
    messages: Vec<(String, String)>, // (speaker, text)
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agents: &[AgentSummary], state: &mut MeetingState) {
    // Header
    ui.heading("🤝 多 Agent 會議室");
    let participants: Vec<&str> = agents.iter().filter(|a| a.enabled).map(|a| a.name.as_str()).collect();
    ui.colored_label(Color32::GRAY, format!("參與者: {}", participants.join(", ")));
    ui.separator();

    // Messages area
    ScrollArea::vertical().id_salt("meeting_msgs").stick_to_bottom(true).auto_shrink(false)
        .max_height(ui.available_height() - 50.0).show(ui, |ui| {
            if state.messages.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label(RichText::new("💬").size(32.0));
                    ui.colored_label(Color32::DARK_GRAY, "輸入訊息開始會議");
                });
            }
            for (speaker, text) in &state.messages {
                egui::Frame::new().fill(Color32::from_rgb(28, 32, 38)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                    ui.colored_label(Color32::from_rgb(100, 180, 255), RichText::new(speaker).small().strong());
                    ui.label(text);
                });
                ui.add_space(4.0);
            }
        });

    // Input bar
    ui.separator();
    ui.horizontal(|ui| {
        let resp = ui.add_sized(
            [ui.available_width() - 60.0, 28.0],
            egui::TextEdit::singleline(&mut state.input).hint_text("輸入訊息..."),
        );
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let send = ui.button("發送").clicked() || enter;

        if send && !state.input.trim().is_empty() {
            let msg = state.input.trim().to_string();
            state.messages.push(("Operator".to_string(), msg.clone()));
            svc.meeting_send("Operator", &msg);
            state.input.clear();
        }
    });
}
