//! Workflow — AI Skill development 6-stage pipeline.

use std::sync::Arc;
use eframe::egui::{self, Color32, RichText, ScrollArea};
use crate::ui_service::*;

#[derive(Default)]
pub struct WorkflowUiState {
    new_feature: String,
    new_description: String,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut WorkflowUiState) {
    ui.heading("Skill 開發工作流");
    ui.add_space(8.0);

    match svc.workflow_state() {
        None => show_empty(ui, svc, state),
        Some(wf) => show_active(ui, svc, &wf),
    }
}

fn show_empty(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut WorkflowUiState) {
    egui::Frame::new().fill(Color32::from_rgb(28, 32, 38)).corner_radius(8.0).inner_margin(16.0).show(ui, |ui| {
        ui.label(RichText::new("開始新的 Skill 開發").strong().size(16.0));
        ui.add_space(8.0);

        ui.label("技能名稱:");
        ui.text_edit_singleline(&mut state.new_feature);
        ui.add_space(4.0);

        ui.label("功能描述:");
        ui.add_sized([ui.available_width(), 60.0],
            egui::TextEdit::multiline(&mut state.new_description).hint_text("描述這個技能要做什麼..."));
        ui.add_space(8.0);

        let can_start = !state.new_feature.trim().is_empty();
        if ui.add_enabled(can_start, egui::Button::new("🚀 開始開發").fill(Color32::from_rgb(40, 80, 160))).clicked() {
            svc.workflow_create(state.new_feature.trim(), state.new_description.trim());
            state.new_feature.clear();
            state.new_description.clear();
        }
    });
}

fn show_active(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, wf: &WorkflowView) {
    // Header
    ui.horizontal(|ui| {
        ui.label(RichText::new(&wf.feature).strong().size(18.0));
        ui.colored_label(Color32::DARK_GRAY, format!("開始於 {}", wf.started_at));
    });
    if !wf.description.is_empty() {
        ui.colored_label(Color32::GRAY, &wf.description);
    }
    ui.add_space(8.0);

    // Stage pipeline (horizontal progress bar)
    ui.horizontal(|ui| {
        for stage in &wf.stages {
            let (bg, fg, icon) = match stage.status {
                StageStatusView::Done => (Color32::from_rgb(20, 50, 25), Color32::from_rgb(100, 220, 100), "✅"),
                StageStatusView::Current => (Color32::from_rgb(20, 40, 70), Color32::from_rgb(120, 180, 255), "▶"),
                StageStatusView::Pending => (Color32::from_rgb(35, 35, 40), Color32::GRAY, "○"),
            };
            egui::Frame::new().fill(bg).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                ui.set_width(ui.available_width() / wf.stages.len().max(1) as f32 - 8.0);
                ui.vertical_centered(|ui| {
                    ui.label(icon);
                    ui.colored_label(fg, RichText::new(&stage.label).small().strong());
                    ui.colored_label(Color32::DARK_GRAY, RichText::new(&stage.desc).small());
                });
            });
        }
    });
    ui.add_space(8.0);

    if wf.all_done {
        ui.colored_label(Color32::from_rgb(100, 220, 100), "🎉 所有階段已完成！");
    }

    ui.add_space(16.0);
    if ui.small_button("🗑 重置工作流").clicked() {
        svc.workflow_reset();
    }
}
