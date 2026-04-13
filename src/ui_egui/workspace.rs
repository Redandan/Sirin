//! Workspace — per-agent view with 4 tabs:
//!   0=概覽 (tasks + memory search)
//!   1=思考流 (agent execution trace)
//!   2=待確認 (pending approvals with draft editing)
//!   3=⚙設定 (per-agent config: identity, objectives, behavior, KPI)

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::theme;
use crate::ui_service::*;

#[derive(Default)]
pub struct WorkspaceState {
    tab: usize, // 0=overview, 1=thinking, 2=pending, 3=settings
    pending_cache: Vec<PendingReplyView>,
    pending_loaded_for: String,
    draft_edits: std::collections::HashMap<String, String>,
    mem_query: String,
    mem_results: Vec<String>,
    // Per-agent settings
    new_objective: String,
}

pub fn show(
    ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agents: &[AgentSummary],
    idx: usize, tasks: &[TaskView],
    pending_counts: &std::collections::HashMap<String, usize>, state: &mut WorkspaceState,
) {
    let Some(agent) = agents.get(idx) else { ui.label("Agent not found"); return; };
    let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);

    // Tab bar (underline style)
    let pending_label = format!("待確認 ({pending_n})");
    let tab_labels = ["概覽", "思考流", &pending_label, "設定"];
    theme::tab_bar(ui, &tab_labels, &mut state.tab);

    match state.tab {
        0 => show_overview(ui, svc, tasks, state),
        1 => show_thinking(ui, tasks),
        2 => show_pending(ui, svc, &agent.id, state),
        3 => show_agent_settings(ui, svc, &agent.id, state),
        _ => {}
    }
}

fn show_overview(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, tasks: &[TaskView], state: &mut WorkspaceState) {
    // Memory search
    ui.horizontal(|ui| {
        ui.label(RichText::new("🔍").color(theme::INFO));
        let resp = ui.text_edit_singleline(&mut state.mem_query);
        if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) || ui.button("搜尋").clicked())
            && !state.mem_query.trim().is_empty() {
            state.mem_results = svc.search_memory(state.mem_query.trim(), 5);
        }
    });

    if !state.mem_results.is_empty() {
        ui.add_space(theme::SP_SM);
        theme::card(ui, |ui| {
            ui.label(RichText::new("搜尋結果").size(theme::FONT_SMALL).strong().color(theme::TEXT_DIM));
            for r in &state.mem_results {
                ui.colored_label(theme::INFO, RichText::new(r).size(theme::FONT_SMALL));
                ui.separator();
            }
        });
    }

    ui.add_space(theme::SP_MD);
    ui.label(RichText::new("近期活動").strong().color(theme::TEXT_DIM));
    if tasks.is_empty() { ui.colored_label(theme::TEXT_DIM, "目前沒有活動記錄"); return; }

    ScrollArea::vertical().id_salt("tasks").show(ui, |ui| {
        for task in tasks.iter().take(30) {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&task.event).size(theme::FONT_BODY).color(theme::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let s = task.status.as_deref().unwrap_or("");
                        theme::badge(ui, s, theme::status_color(s));
                    });
                });
                if let Some(r) = &task.reason { ui.colored_label(theme::TEXT_DIM, RichText::new(r).size(theme::FONT_SMALL)); }
                ui.colored_label(theme::TEXT_DIM, RichText::new(&task.timestamp).size(theme::FONT_SMALL));
            });
        }
    });
}

fn show_thinking(ui: &mut egui::Ui, tasks: &[TaskView]) {
    ui.label(RichText::new("Agent 執行追蹤").strong().color(theme::TEXT_DIM));
    ui.add_space(theme::SP_SM);

    let thinking: Vec<&TaskView> = tasks.iter()
        .filter(|t| ["adk", "chat", "research", "coding", "router", "planner"].iter().any(|k| t.event.contains(k)))
        .take(50).collect();

    if thinking.is_empty() { ui.colored_label(theme::TEXT_DIM, "暫無執行記錄"); return; }

    ScrollArea::vertical().id_salt("thinking").show(ui, |ui| {
        for task in thinking {
            let icon = if task.event.contains("research") { "🔍" }
                else if task.event.contains("coding") { "💻" }
                else if task.event.contains("chat") { "💬" }
                else { "🧠" };
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(icon);
                    ui.label(RichText::new(&task.event).size(theme::FONT_BODY).color(theme::INFO));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(theme::TEXT_DIM, RichText::new(&task.timestamp).size(theme::FONT_SMALL));
                    });
                });
                if let Some(r) = &task.reason { ui.colored_label(theme::TEXT_DIM, RichText::new(r).size(theme::FONT_SMALL)); }
            });
        }
    });
}

fn show_pending(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agent_id: &str, state: &mut WorkspaceState) {
    if state.pending_loaded_for != agent_id {
        state.pending_cache = svc.load_pending(agent_id);
        state.pending_loaded_for = agent_id.to_string();
    }
    if state.pending_cache.is_empty() {
        theme::badge(ui, "✅ 沒有待確認的回覆", theme::ACCENT);
        return;
    }

    // Collect reply IDs for edit buffers
    for reply in &state.pending_cache {
        state.draft_edits.entry(reply.id.clone()).or_insert_with(|| reply.draft_reply.clone());
    }

    ScrollArea::vertical().id_salt("pending").show(ui, |ui| {
        let mut action: Option<(String, u8)> = None; // 1=approve, 2=reject, 3=save_edit
        let reply_ids: Vec<String> = state.pending_cache.iter().map(|r| r.id.clone()).collect();

        for reply in &state.pending_cache {
            theme::card(ui, |ui| {
                // Header: from + timestamp
                ui.horizontal(|ui| {
                    ui.colored_label(theme::TEXT_DIM, "來自");
                    ui.colored_label(theme::INFO, &reply.peer_name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(theme::TEXT_DIM, RichText::new(&reply.created_at).size(theme::FONT_SMALL));
                    });
                });
                ui.add_space(theme::SP_SM);

                // Original message (read-only)
                egui::Frame::new().fill(theme::BG).corner_radius(6.0).inner_margin(theme::SP_MD).show(ui, |ui| {
                    ui.label(RichText::new(&reply.original_message).size(theme::FONT_BODY).color(theme::TEXT_DIM));
                });
                ui.add_space(theme::SP_SM);

                // Draft reply (EDITABLE)
                ui.label(RichText::new("✏ 草稿（可編輯）").size(theme::FONT_SMALL).color(theme::TEXT_DIM));
                if let Some(buf) = state.draft_edits.get_mut(&reply.id) {
                    egui::Frame::new().fill(theme::INFO.linear_multiply(0.06)).corner_radius(6.0).inner_margin(theme::SP_MD)
                        .stroke(egui::Stroke::new(1.0, theme::INFO.linear_multiply(0.2)))
                        .show(ui, |ui| {
                            ui.add_sized([ui.available_width(), 60.0],
                                egui::TextEdit::multiline(buf).text_color(theme::INFO).font(egui::TextStyle::Body));
                        });
                }
                ui.add_space(theme::SP_MD);

                // Action buttons
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("✓ 核准送出").color(theme::BG)).fill(theme::ACCENT).corner_radius(6.0)).clicked() {
                        action = Some((reply.id.clone(), 1));
                    }
                    if ui.add(egui::Button::new(RichText::new("✗ 拒絕").color(theme::BG)).fill(theme::DANGER).corner_radius(6.0)).clicked() {
                        action = Some((reply.id.clone(), 2));
                    }
                    if ui.add(egui::Button::new(RichText::new("💾 儲存修改").color(theme::TEXT)).fill(theme::HOVER).corner_radius(6.0)).clicked() {
                        action = Some((reply.id.clone(), 3));
                    }
                });
            });
        }

        if let Some((id, act)) = action {
            match act {
                1 => { // Approve: save edit first, then approve
                    if let Some(edited) = state.draft_edits.get(&id) {
                        svc.edit_draft(agent_id, &id, edited);
                    }
                    svc.approve_reply(agent_id, &id);
                }
                2 => { svc.reject_reply(agent_id, &id); }
                3 => { // Save edit only
                    if let Some(edited) = state.draft_edits.get(&id) {
                        svc.edit_draft(agent_id, &id, edited);
                    }
                }
                _ => {}
            }
            state.pending_cache = svc.load_pending(agent_id);
            // Clean up stale edit buffers
            state.draft_edits.retain(|k, _| reply_ids.contains(k));
        }
    });
}

// ── Per-agent settings tab — editable form ───────────────────────────────────

fn show_agent_settings(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agent_id: &str, state: &mut WorkspaceState) {
    let Some(d) = svc.agent_detail(agent_id) else {
        ui.colored_label(theme::DANGER, "無法載入 Agent 設定");
        return;
    };

    ScrollArea::vertical().id_salt("agent_settings").show(ui, |ui| {
        // ── Header ───────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.label(RichText::new(&d.name).size(theme::FONT_HEADING).strong().color(theme::TEXT));
            ui.add_space(theme::SP_MD);
            let mut enabled = d.enabled;
            if ui.checkbox(&mut enabled, "").changed() {
                svc.toggle_agent(&d.id, enabled);
            }
            theme::badge(ui, if d.enabled { "啟用" } else { "停用" }, if d.enabled { theme::ACCENT } else { theme::TEXT_DIM });
        });
        ui.colored_label(theme::TEXT_DIM, RichText::new(format!("ID: {}", d.id)).size(theme::FONT_CAPTION));
        ui.add_space(theme::SP_LG);

        // ── 身份 ─────────────────────────────────────────────────────────
        theme::section(ui, "身份", |ui| {
            form_row(ui, "語氣", &d.professional_tone);
            ui.add_space(theme::SP_XS);
            // Remote AI toggle
            ui.horizontal(|ui| {
                ui.colored_label(theme::TEXT_DIM, RichText::new("遠端 AI").size(theme::FONT_SMALL));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut allowed = !d.disable_remote_ai;
                    if ui.checkbox(&mut allowed, "允許").changed() {
                        svc.set_remote_ai(agent_id, allowed);
                    }
                });
            });
        });

        // ── 目標 ─────────────────────────────────────────────────────────
        theme::section(ui, "目標", |ui| {
            if d.objectives.is_empty() {
                ui.colored_label(theme::TEXT_DIM, RichText::new("尚未設定（使用全域 Persona 目標）").size(theme::FONT_SMALL));
            }
            let mut remove_idx: Option<usize> = None;
            for (i, obj) in d.objectives.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.colored_label(theme::INFO, "•");
                    ui.label(RichText::new(obj).size(theme::FONT_BODY).color(theme::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new("✕").size(theme::FONT_CAPTION).color(theme::TEXT_DIM))
                            .fill(egui::Color32::TRANSPARENT).corner_radius(4.0))
                            .on_hover_text("刪除").clicked() {
                            remove_idx = Some(i);
                        }
                    });
                });
            }
            if let Some(idx) = remove_idx { svc.remove_objective(agent_id, idx); }

            ui.add_space(theme::SP_SM);
            ui.horizontal(|ui| {
                ui.add_sized([ui.available_width() - 56.0, 26.0],
                    egui::TextEdit::singleline(&mut state.new_objective).hint_text("輸入新目標..."));
                if ui.add(egui::Button::new(RichText::new("+ 新增").size(theme::FONT_SMALL).color(theme::BG))
                    .fill(theme::INFO).corner_radius(4.0)).clicked()
                    && !state.new_objective.trim().is_empty() {
                    svc.add_objective(agent_id, state.new_objective.trim());
                    state.new_objective.clear();
                }
            });
        });

        // ── 回覆行為（可調整）────────────────────────────────────────────
        theme::section(ui, "回覆行為", |ui| {
            let mut beh_enabled = d.human_behavior_enabled;
            let mut min_d = d.min_reply_delay as f32;
            let mut max_d = d.max_reply_delay as f32;
            let mut max_h = d.max_per_hour as f32;
            let mut max_day = d.max_per_day as f32;
            let mut changed = false;

            ui.horizontal(|ui| {
                ui.colored_label(theme::TEXT_DIM, RichText::new("啟用").size(theme::FONT_SMALL));
                if ui.checkbox(&mut beh_enabled, "").changed() { changed = true; }
            });

            ui.add_space(theme::SP_XS);
            ui.horizontal(|ui| {
                ui.colored_label(theme::TEXT_DIM, RichText::new("最小延遲").size(theme::FONT_SMALL));
                ui.add_space(theme::SP_SM);
                if ui.add(egui::Slider::new(&mut min_d, 0.0..=300.0).suffix("s").integer()).changed() { changed = true; }
            });
            ui.horizontal(|ui| {
                ui.colored_label(theme::TEXT_DIM, RichText::new("最大延遲").size(theme::FONT_SMALL));
                ui.add_space(theme::SP_SM);
                if ui.add(egui::Slider::new(&mut max_d, 0.0..=600.0).suffix("s").integer()).changed() { changed = true; }
            });
            ui.horizontal(|ui| {
                ui.colored_label(theme::TEXT_DIM, RichText::new("每小時上限").size(theme::FONT_SMALL));
                if ui.add(egui::Slider::new(&mut max_h, 0.0..=100.0).integer()).changed() { changed = true; }
            });
            ui.horizontal(|ui| {
                ui.colored_label(theme::TEXT_DIM, RichText::new("每日上限").size(theme::FONT_SMALL));
                ui.add_space(theme::SP_SM);
                if ui.add(egui::Slider::new(&mut max_day, 0.0..=500.0).integer()).changed() { changed = true; }
            });

            if changed {
                svc.set_behavior(agent_id, beh_enabled, min_d as u64, max_d as u64, max_h as u32, max_day as u32);
            }
        });

        // ── 通道 ─────────────────────────────────────────────────────────
        theme::section(ui, "通道", |ui| {
            form_row(ui, "平台", &d.platform);
            form_row(ui, "ID", &d.id);
        });

        // ── KPI ──────────────────────────────────────────────────────────
        if !d.kpi_labels.is_empty() {
            theme::section(ui, "KPI 指標", |ui| {
                for (label, unit) in &d.kpi_labels {
                    theme::info_row(ui, label, unit);
                }
            });
        }

        // ── 危險操作 ─────────────────────────────────────────────────────
        ui.add_space(theme::SP_XL);
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("危險操作").size(theme::FONT_CAPTION));
        });
        ui.add_space(theme::SP_XS);
        if ui.add(egui::Button::new(RichText::new("🗑 刪除此 Agent").size(theme::FONT_SMALL).color(theme::DANGER))
            .fill(theme::DANGER.linear_multiply(0.1)).corner_radius(4.0)).clicked() {
            svc.delete_agent(agent_id);
        }
    });
}

/// A label-value form row (read-only).
fn form_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.colored_label(theme::TEXT_DIM, RichText::new(label).size(theme::FONT_SMALL));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).size(theme::FONT_BODY).color(theme::TEXT));
        });
    });
}
