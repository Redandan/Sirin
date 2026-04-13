//! Workflow — Skill development 6-stage pipeline with AI integration.

use std::sync::Arc;
use eframe::egui::{self, RichText};
use super::theme;
use crate::ui_service::*;

#[derive(Default)]
pub struct WorkflowUiState {
    new_feature: String,
    new_description: String,
    stage_prompt: Option<String>,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut WorkflowUiState) {
    ui.add_space(theme::SP_SM);
    match svc.workflow_state() {
        None => show_empty(ui, svc, state),
        Some(wf) => show_active(ui, svc, &wf, state),
    }
}

fn show_empty(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut WorkflowUiState) {
    theme::card(ui, |ui| {
        ui.label(RichText::new("開始新的 Skill 開發").strong().size(16.0).color(theme::TEXT));
        ui.add_space(theme::SP_MD);
        ui.label(RichText::new("技能名稱:").color(theme::SUBTEXT0));
        ui.text_edit_singleline(&mut state.new_feature);
        ui.add_space(theme::SP_SM);
        ui.label(RichText::new("功能描述:").color(theme::SUBTEXT0));
        ui.add_sized([ui.available_width(), 60.0],
            egui::TextEdit::multiline(&mut state.new_description).hint_text("描述技能功能..."));
        ui.add_space(theme::SP_MD);
        let can = !state.new_feature.trim().is_empty();
        if ui.add_enabled(can, egui::Button::new(RichText::new("🚀 開始開發").color(theme::CRUST)).fill(theme::BLUE).corner_radius(6.0)).clicked() {
            svc.workflow_create(state.new_feature.trim(), state.new_description.trim());
            state.new_feature.clear();
            state.new_description.clear();
            state.stage_prompt = svc.workflow_stage_prompt();
        }
    });
}

fn show_active(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, wf: &WorkflowView, state: &mut WorkflowUiState) {
    // Header
    ui.horizontal(|ui| {
        ui.label(RichText::new(&wf.feature).strong().size(18.0).color(theme::TEXT));
        ui.colored_label(theme::OVERLAY0, format!("({}) 開始於 {}", wf.skill_id, wf.started_at));
    });
    if !wf.description.is_empty() { ui.colored_label(theme::SUBTEXT0, &wf.description); }
    ui.add_space(theme::SP_LG);

    // Pipeline stages
    ui.horizontal(|ui| {
        for stage in &wf.stages {
            let (bg, fg, icon) = match stage.status {
                StageStatusView::Done => (theme::GREEN.linear_multiply(0.12), theme::GREEN, "✅"),
                StageStatusView::Current => (theme::BLUE.linear_multiply(0.12), theme::BLUE, "▶"),
                StageStatusView::Pending => (theme::SURFACE0, theme::OVERLAY0, "○"),
            };
            egui::Frame::new().fill(bg).corner_radius(theme::CARD_RADIUS)
                .inner_margin(theme::SP_MD)
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
    ui.add_space(theme::SP_LG);

    if wf.all_done {
        ui.colored_label(theme::GREEN, "🎉 所有階段已完成！");
    } else {
        // Current stage controls
        theme::section(ui, &format!("當前: {}", wf.current_stage), |ui| {
            // Show stage prompt (for AI reference)
            if state.stage_prompt.is_none() {
                state.stage_prompt = svc.workflow_stage_prompt();
            }
            if let Some(prompt) = &state.stage_prompt {
                egui::CollapsingHeader::new(RichText::new("📄 AI Prompt（可複製）").small().color(theme::OVERLAY0))
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::Frame::new().fill(theme::CRUST).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
                            ui.colored_label(theme::SUBTEXT0, RichText::new(prompt).monospace().small());
                        });
                        if ui.small_button("📋 複製 Prompt").clicked() {
                            ui.ctx().copy_text(prompt.clone());
                        }
                    });
            }

            ui.add_space(theme::SP_MD);
            if ui.add(egui::Button::new(RichText::new("✅ 完成此階段 → 下一步").color(theme::CRUST)).fill(theme::GREEN).corner_radius(6.0)).clicked() {
                svc.workflow_advance();
                state.stage_prompt = svc.workflow_stage_prompt();
            }
        });
    }

    ui.add_space(theme::SP_XL);
    if ui.small_button("🗑 重置工作流").clicked() {
        svc.workflow_reset();
        state.stage_prompt = None;
    }
}
