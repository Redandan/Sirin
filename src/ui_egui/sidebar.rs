//! Left sidebar — agent list + navigation.
//!
//! AI reads this: a 215px wide panel with agent cards (name, status dot, pending badge)
//! and bottom nav buttons (Settings, Log).

use eframe::egui::{self, Color32, RichText, ScrollArea};

use super::View;
use crate::ui_service::AgentSummary;

pub fn show(
    ctx: &egui::Context,
    agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View,
) {
    egui::SidePanel::left("sidebar")
        .resizable(false)
        .exact_width(215.0)
        .show(ctx, |ui| {
            ui.add_space(6.0);
            ui.label(RichText::new("Sirin").heading().strong());
            ui.label(RichText::new("AI Agent Platform").small().color(Color32::GRAY));
            ui.add_space(4.0);
            ui.separator();

            // Agent list
            ScrollArea::vertical()
                .id_salt("sidebar_agents")
                .max_height(ui.available_height() - 80.0)
                .show(ui, |ui| {
                    for (idx, agent) in agents.iter().enumerate() {
                        let is_selected = matches!(view, View::Workspace(i) if *i == idx);
                        let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
                        agent_card(ui, agent, is_selected, pending_n, || {
                            *view = View::Workspace(idx);
                        });
                    }
                });

            ui.separator();

            // Bottom nav
            nav_button(ui, "⚙ 設定", matches!(view, View::Settings), || *view = View::Settings);
            nav_button(ui, "📋 Log", matches!(view, View::Log), || *view = View::Log);
        });
}

fn agent_card(
    ui: &mut egui::Ui,
    agent: &AgentSummary,
    selected: bool,
    pending: usize,
    on_click: impl FnOnce(),
) {
    let bg = if selected {
        Color32::from_rgb(30, 55, 90)
    } else {
        Color32::TRANSPARENT
    };

    let frame = egui::Frame::new()
        .fill(bg)
        .corner_radius(6.0)
        .inner_margin(8.0);

    frame.show(ui, |ui| {
        let response = ui.interact(
            ui.max_rect(),
            ui.id().with(&agent.id),
            egui::Sense::click(),
        );
        if response.clicked() {
            on_click();
        }

        ui.horizontal(|ui| {
            // Agent name
            ui.label(RichText::new(&agent.name).strong().size(14.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Pending badge
                if pending > 0 {
                    let badge = egui::Frame::new()
                        .fill(Color32::from_rgb(210, 100, 20))
                        .corner_radius(10.0)
                        .inner_margin(egui::vec2(5.0, 1.0));
                    badge.show(ui, |ui| {
                        ui.label(RichText::new(format!("{pending}")).small().color(Color32::WHITE));
                    });
                }
            });
        });

        ui.horizontal(|ui| {
            // Status dot
            let (dot, color) = if agent.enabled {
                ("●", Color32::from_rgb(80, 200, 100))
            } else {
                ("○", Color32::GRAY)
            };
            ui.colored_label(color, RichText::new(dot).small());
            ui.colored_label(Color32::GRAY, RichText::new(&agent.id).small());
        });
    });
    ui.add_space(2.0);
}

fn nav_button(ui: &mut egui::Ui, label: &str, active: bool, on_click: impl FnOnce()) {
    let bg = if active { Color32::from_rgb(50, 50, 55) } else { Color32::TRANSPARENT };
    let btn = egui::Button::new(RichText::new(label).size(13.0))
        .fill(bg)
        .corner_radius(4.0);
    if ui.add_sized([ui.available_width(), 28.0], btn).clicked() {
        on_click();
    }
}
