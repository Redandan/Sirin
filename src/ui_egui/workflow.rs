//! Workflow — Skill development 6-stage pipeline with Catppuccin theme.

use std::sync::Arc;
use eframe::egui::{self, RichText};
use super::theme;
use crate::ui_service::*;

#[derive(Default)]
pub struct WorkflowUiState {
    new_feature: String,
    new_description: String,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut WorkflowUiState) {
    ui.label(RichText::new("Skill 開發工作流").heading().strong().color(theme::TEXT));
    ui.add_space(theme::GAP_MD);

    match svc.workflow_state() {
        None => show_empty(ui, svc, state),
        Some(wf) => show_active(ui, svc, &wf),
    }
}

fn show_empty(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut WorkflowUiState) {
    theme::card(ui, |ui| {
        ui.label(RichText::new("開始新的 Skill 開發").strong().size(16.0).color(theme::TEXT));
        ui.add_space(theme::GAP_MD);
        ui.label(RichText::new("技能名稱:").color(theme::SUBTEXT0));
        ui.text_edit_singleline(&mut state.new_feature);
        ui.add_space(theme::GAP_SM);
        ui.label(RichText::new("功能描述:").color(theme::SUBTEXT0));
        ui.add_sized([ui.available_width(), 60.0],
            egui::TextEdit::multiline(&mut state.new_description).hint_text("描述技能功能..."));
        ui.add_space(theme::GAP_MD);
        let can = !state.new_feature.trim().is_empty();
        if ui.add_enabled(can, egui::Button::new(RichText::new("🚀 開始開發").color(theme::CRUST))
            .fill(theme::BLUE).corner_radius(6.0)).clicked() {
            svc.workflow_create(state.new_feature.trim(), state.new_description.trim());
            state.new_feature.clear();
            state.new_description.clear();
        }
    });
}

fn show_active(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, wf: &WorkflowView) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(&wf.feature).strong().size(18.0).color(theme::TEXT));
        ui.colored_label(theme::OVERLAY0, format!("開始於 {}", wf.started_at));
    });
    if !wf.description.is_empty() { ui.colored_label(theme::SUBTEXT0, &wf.description); }
    ui.add_space(theme::GAP_LG);

    // Pipeline stages
    ui.horizontal(|ui| {
        for stage in &wf.stages {
            let (bg, fg, icon) = match stage.status {
                StageStatusView::Done => (theme::GREEN.linear_multiply(0.12), theme::GREEN, "✅"),
                StageStatusView::Current => (theme::BLUE.linear_multiply(0.12), theme::BLUE, "▶"),
                StageStatusView::Pending => (theme::SURFACE0, theme::OVERLAY0, "○"),
            };
            egui::Frame::new().fill(bg).corner_radius(theme::CARD_RADIUS)
                .inner_margin(theme::GAP_MD)
                .stroke(egui::Stroke::new(1.0, fg.linear_multiply(0.3)))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width() / wf.stages.len().max(1) as f32 - 8.0);
                    ui.vertical_centered(|ui| {
                        ui.label(icon);
                        ui.colored_label(fg, RichText::new(&stage.label).small().strong());
                        ui.colored_label(theme::OVERLAY0, RichText::new(&stage.desc).small());
                    });
                });
        }
    });
    ui.add_space(theme::GAP_MD);

    if wf.all_done { ui.colored_label(theme::GREEN, "🎉 所有階段已完成！"); }

    ui.add_space(theme::GAP_XL);
    if ui.small_button("🗑 重置工作流").clicked() { svc.workflow_reset(); }
}
