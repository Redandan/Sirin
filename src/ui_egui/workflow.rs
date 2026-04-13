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
    ai_output: String,
    ai_loading: bool,
    ai_rx: Option<std::sync::mpsc::Receiver<String>>,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut WorkflowUiState) {
    ui.set_max_width(600.0);

    // Poll async AI generation
    if let Some(rx) = &state.ai_rx {
        if let Ok(result) = rx.try_recv() {
            state.ai_output = result;
            state.ai_loading = false;
            state.ai_rx = None;
        }
    }

    match svc.workflow_state() {
        None => show_empty(ui, svc, state),
        Some(wf) => show_active(ui, svc, &wf, state),
    }
}

fn show_empty(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut WorkflowUiState) {
    theme::card(ui, |ui| {
        ui.label(RichText::new("開始新的 Skill 開發").strong().size(theme::FONT_HEADING).color(theme::TEXT));
        ui.add_space(theme::SP_MD);
        ui.label(RichText::new("技能名稱:").color(theme::TEXT_DIM));
        ui.text_edit_singleline(&mut state.new_feature);
        ui.add_space(theme::SP_SM);
        ui.label(RichText::new("功能描述:").color(theme::TEXT_DIM));
        ui.add_sized([ui.available_width(), 60.0],
            egui::TextEdit::multiline(&mut state.new_description).hint_text("描述技能功能..."));
        ui.add_space(theme::SP_MD);
        let can = !state.new_feature.trim().is_empty();
        if ui.add_enabled(can, egui::Button::new(RichText::new("🚀 開始開發").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked() {
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
        ui.label(RichText::new(&wf.feature).strong().size(theme::FONT_HEADING).color(theme::TEXT));
        ui.colored_label(theme::TEXT_DIM, format!("({}) 開始於 {}", wf.skill_id, wf.started_at));
    });
    if !wf.description.is_empty() { ui.colored_label(theme::TEXT_DIM, &wf.description); }
    ui.add_space(theme::SP_LG);

    // Pipeline stages
    ui.horizontal(|ui| {
        for stage in &wf.stages {
            let (bg, fg, icon) = match stage.status {
                StageStatusView::Done => (theme::ACCENT.linear_multiply(0.12), theme::ACCENT, "✅"),
                StageStatusView::Current => (theme::INFO.linear_multiply(0.12), theme::INFO, "▶"),
                StageStatusView::Pending => (theme::CARD, theme::TEXT_DIM, "○"),
            };
            egui::Frame::new().fill(bg).corner_radius(4.0)
                .inner_margin(theme::SP_MD)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width() / wf.stages.len().max(1) as f32 - 8.0);
                    ui.vertical_centered(|ui| {
                        ui.label(icon);
                        ui.colored_label(fg, RichText::new(&stage.label).size(theme::FONT_SMALL).strong());
                        ui.colored_label(theme::TEXT_DIM, RichText::new(&stage.desc).size(theme::FONT_SMALL));
                    });
                });
        }
    });
    ui.add_space(theme::SP_LG);

    if wf.all_done {
        ui.colored_label(theme::ACCENT, "🎉 所有階段已完成！");
    } else {
        theme::section(ui, &format!("當前: {}", wf.current_stage), |ui| {
            if state.stage_prompt.is_none() { state.stage_prompt = svc.workflow_stage_prompt(); }

            // AI generate
            ui.horizontal(|ui| {
                let can = !state.ai_loading;
                if ui.add_enabled(can, egui::Button::new(RichText::new("🤖 AI 生成").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked() {
                    state.ai_loading = true;
                    let (tx, rx) = std::sync::mpsc::channel();
                    let svc = svc.clone();
                    std::thread::spawn(move || { let _ = tx.send(svc.workflow_generate().unwrap_or_else(|| "（生成失敗）".into())); });
                    state.ai_rx = Some(rx);
                }
                if state.ai_loading { ui.colored_label(theme::YELLOW, "● 生成中..."); }
                if ui.small_button("📋 複製 Prompt").clicked() {
                    if let Some(p) = &state.stage_prompt { ui.ctx().copy_text(p.clone()); }
                }
            });

            // AI output
            if !state.ai_output.is_empty() {
                ui.add_space(theme::SP_SM);
                ui.colored_label(theme::TEXT_DIM, RichText::new("AI 輸出（可編輯）:").size(theme::FONT_SMALL));
                ui.add_sized([ui.available_width(), 120.0], egui::TextEdit::multiline(&mut state.ai_output).font(egui::TextStyle::Monospace));
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("💾 接受").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked() {
                        svc.workflow_save_output(&wf.current_stage, &state.ai_output);
                    }
                });
            }

            // Prompt ref
            if let Some(prompt) = &state.stage_prompt {
                egui::CollapsingHeader::new(RichText::new("📄 Prompt").size(theme::FONT_CAPTION).color(theme::TEXT_DIM))
                    .default_open(false).show(ui, |ui| {
                        ui.colored_label(theme::TEXT_DIM, RichText::new(prompt).monospace().size(theme::FONT_CAPTION));
                    });
            }

            ui.add_space(theme::SP_MD);
            if ui.add(egui::Button::new(RichText::new("✅ 下一步").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked() {
                svc.workflow_advance(); state.stage_prompt = svc.workflow_stage_prompt(); state.ai_output.clear();
            }
        });
    }

    ui.add_space(theme::SP_XL);
    if ui.small_button("🗑 重置工作流").clicked() {
        svc.workflow_reset();
        state.stage_prompt = None;
    }
}
