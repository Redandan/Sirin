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
    tab: usize, // 0=chat, 1=overview, 2=thinking, 3=pending, 4=settings
    pending_cache: Vec<PendingReplyView>,
    pending_loaded_for: String,
    draft_edits: std::collections::HashMap<String, String>,
    mem_query: String,
    mem_results: Vec<String>,
    new_objective: String,
    // Chat
    chat_input: String,
    chat_history: Vec<(String, String)>,
    chat_loading: bool,
    chat_rx: Option<std::sync::mpsc::Receiver<String>>,
    // Delete confirmation (agent_id being confirmed)
    delete_confirming: bool,
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
    let tab_labels = ["💬 對話", "概覽", "思考流", &pending_label, "設定"];
    theme::tab_bar(ui, &tab_labels, &mut state.tab);

    match state.tab {
        0 => show_chat(ui, svc, &agent.id, state),
        1 => show_overview(ui, svc, tasks, state),
        2 => show_thinking(ui, tasks),
        3 => show_pending(ui, svc, &agent.id, state),
        4 => show_agent_settings(ui, svc, &agent.id, state),
        _ => {}
    }
}

fn show_chat(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agent_id: &str, state: &mut WorkspaceState) {
    // Poll for async reply
    if let Some(rx) = &state.chat_rx {
        if let Ok(reply) = rx.try_recv() {
            state.chat_history.push(("agent".into(), reply));
            state.chat_loading = false;
            state.chat_rx = None;
        }
    }

    // Message history
    ScrollArea::vertical().id_salt("chat").stick_to_bottom(true).auto_shrink(false)
        .max_height(ui.available_height() - 50.0).show(ui, |ui| {
            ui.set_max_width(600.0);
            if state.chat_history.is_empty() && !state.chat_loading {
                ui.add_space(theme::SP_XL);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("💬").size(theme::SP_XL));
                    ui.add_space(theme::SP_SM);
                    ui.colored_label(theme::TEXT, RichText::new(format!("與 Agent 對話")).size(theme::FONT_HEADING));
                    ui.add_space(theme::SP_XS);
                    ui.colored_label(theme::TEXT_DIM, "輸入訊息開始對話，Agent 會使用 AI 回覆");
                });
            }
            for (role, text) in &state.chat_history {
                let (color, prefix) = if role == "user" { (theme::TEXT, "你") } else { (theme::ACCENT, "Agent") };
                theme::card(ui, |ui| {
                    ui.colored_label(color, RichText::new(prefix).size(theme::FONT_SMALL).strong());
                    ui.label(RichText::new(text).size(theme::FONT_BODY).color(theme::TEXT));
                });
            }
            if state.chat_loading {
                ui.colored_label(theme::YELLOW, "● Agent 思考中...");
            }
        });

    // Input bar
    ui.add_space(theme::SP_SM);
    ui.horizontal(|ui| {
        let input_width = (ui.available_width() - 60.0).min(540.0);
        let resp = ui.add_sized([input_width, 28.0],
            egui::TextEdit::singleline(&mut state.chat_input).hint_text("輸入訊息..."));
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let can_send = !state.chat_input.trim().is_empty() && !state.chat_loading;
        if ui.add_enabled(can_send, egui::Button::new(RichText::new("發送").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked() || (enter && can_send)
        {
            let msg = state.chat_input.trim().to_string();
            state.chat_history.push(("user".into(), msg.clone()));
            state.chat_input.clear();
            state.chat_loading = true;

            // Async: spawn background thread, UI continues
            let (tx, rx) = std::sync::mpsc::channel();
            let svc = svc.clone();
            let aid = agent_id.to_string();
            std::thread::spawn(move || {
                let reply = svc.chat_send(&aid, &msg);
                let _ = tx.send(reply);
            });
            state.chat_rx = Some(rx);
        }
    });
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
    if tasks.is_empty() {
        ui.add_space(theme::SP_XL);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("📊").size(theme::SP_XL));
            ui.colored_label(theme::TEXT_DIM, "目前沒有活動記錄");
            ui.colored_label(theme::TEXT_DIM, RichText::new("Agent 執行任務後會顯示在這裡").size(theme::FONT_SMALL));
        });
        return;
    }

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

    if thinking.is_empty() {
        ui.add_space(theme::SP_XL);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("🧠").size(theme::SP_XL));
            ui.colored_label(theme::TEXT_DIM, "暫無 Agent 執行記錄");
            ui.colored_label(theme::TEXT_DIM, RichText::new("Agent 處理訊息時會顯示推理過程").size(theme::FONT_SMALL));
        });
        return;
    }

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
                    egui::Frame::new().fill(theme::INFO.linear_multiply(0.06)).corner_radius(4.0).inner_margin(theme::SP_MD)
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

// ── Per-agent settings tab ───────────────────────────────────────────────────
//
// Layout: max-width 500px content area, Claude Desktop style sections.
// Each row: fixed 100px label + value next to it (no right-to-left stretch).

fn show_agent_settings(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agent_id: &str, state: &mut WorkspaceState) {
    let Some(d) = svc.agent_detail(agent_id) else {
        ui.colored_label(theme::DANGER, "無法載入 Agent 設定");
        return;
    };

    ScrollArea::vertical().id_salt("agent_settings").show(ui, |ui| {
        // Constrain content width for readability (like Claude Desktop)
        ui.set_max_width(520.0);

        // ── Header ───────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            let mut enabled = d.enabled;
            if ui.checkbox(&mut enabled, "").changed() {
                svc.toggle_agent(&d.id, enabled);
            }
            ui.label(RichText::new(&d.name).size(theme::FONT_TITLE).strong().color(theme::TEXT));
        });
        ui.colored_label(theme::TEXT_DIM, RichText::new(format!("ID: {}", d.id)).size(theme::FONT_CAPTION));

        // ── 身份 ─────────────────────────────────────────────────────────
        theme::section(ui, "身份", |ui| {
            setting_row(ui, "語氣", |ui| {
                ui.label(RichText::new(&d.professional_tone).size(theme::FONT_BODY).color(theme::TEXT));
            });
            setting_row(ui, "遠端 AI", |ui| {
                let mut allowed = !d.disable_remote_ai;
                if ui.checkbox(&mut allowed, "允許").changed() {
                    svc.set_remote_ai(agent_id, allowed);
                }
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
                    ui.colored_label(theme::ACCENT, "•");
                    ui.label(RichText::new(obj).size(theme::FONT_BODY).color(theme::TEXT));
                    let del = ui.add(
                        egui::Label::new(RichText::new("  x").size(theme::FONT_BODY).color(theme::DANGER.linear_multiply(0.6)))
                            .selectable(false).sense(egui::Sense::click()),
                    );
                    if del.hovered() {
                        ui.painter().rect_filled(del.rect, 2.0, theme::DANGER.linear_multiply(0.1));
                    }
                    if del.on_hover_text("刪除此目標").clicked() { remove_idx = Some(i); }
                });
            }
            if let Some(idx) = remove_idx { svc.remove_objective(agent_id, idx); }

            ui.add_space(theme::SP_SM);
            ui.horizontal(|ui| {
                let input_width = (ui.available_width() - 70.0).min(350.0);
                ui.add_sized([input_width, 26.0],
                    egui::TextEdit::singleline(&mut state.new_objective).hint_text("輸入新目標..."));
                if ui.add(egui::Button::new(RichText::new("+ 新增").size(theme::FONT_SMALL).color(theme::BG))
                    .fill(theme::ACCENT).corner_radius(4.0)).clicked()
                    && !state.new_objective.trim().is_empty() {
                    svc.add_objective(agent_id, state.new_objective.trim());
                    state.new_objective.clear();
                }
            });
        });

        // ── 回覆行為 ─────────────────────────────────────────────────────
        theme::section(ui, "回覆行為", |ui| {
            let mut beh_enabled = d.human_behavior_enabled;
            let mut min_d = d.min_reply_delay as f32;
            let mut max_d = d.max_reply_delay as f32;
            let mut max_h = d.max_per_hour as f32;
            let mut max_day = d.max_per_day as f32;
            let mut changed = false;

            setting_row(ui, "啟用", |ui| {
                if ui.checkbox(&mut beh_enabled, "").changed() { changed = true; }
            });
            setting_row(ui, "最小延遲", |ui| {
                if ui.add(egui::Slider::new(&mut min_d, 0.0..=300.0).suffix("s").integer()).changed() { changed = true; }
            });
            setting_row(ui, "最大延遲", |ui| {
                if ui.add(egui::Slider::new(&mut max_d, 0.0..=600.0).suffix("s").integer()).changed() { changed = true; }
            });
            setting_row(ui, "每小時上限", |ui| {
                if ui.add(egui::Slider::new(&mut max_h, 0.0..=100.0).integer()).changed() { changed = true; }
            });
            setting_row(ui, "每日上限", |ui| {
                if ui.add(egui::Slider::new(&mut max_day, 0.0..=500.0).integer()).changed() { changed = true; }
            });

            if changed {
                svc.set_behavior(agent_id, beh_enabled, min_d as u64, max_d as u64, max_h as u32, max_day as u32);
            }
        });

        // ── 通道 ─────────────────────────────────────────────────────────
        theme::section(ui, "通道", |ui| {
            setting_row(ui, "平台", |ui| {
                ui.label(RichText::new(&d.platform).size(theme::FONT_BODY).color(theme::TEXT));
            });
            setting_row(ui, "ID", |ui| {
                ui.label(RichText::new(&d.id).size(theme::FONT_BODY).color(theme::TEXT).family(egui::FontFamily::Monospace));
            });
        });

        // ── KPI ──────────────────────────────────────────────────────────
        if !d.kpi_labels.is_empty() {
            theme::section(ui, "KPI 指標", |ui| {
                for (label, unit) in &d.kpi_labels {
                    setting_row(ui, label, |ui| {
                        ui.label(RichText::new(unit).size(theme::FONT_BODY).color(theme::TEXT));
                    });
                }
            });
        }

        // ── 技能控制 ─────────────────────────────────────────────────────
        let skills = svc.system_status().skills;
        if !skills.is_empty() {
            let disabled = svc.disabled_skills(agent_id);
            theme::section(ui, "技能", |ui| {
                for skill in &skills {
                    ui.horizontal(|ui| {
                        let is_enabled = !disabled.contains(&skill.name);
                        let mut checked = is_enabled;
                        if ui.checkbox(&mut checked, "").changed() {
                            svc.toggle_skill(agent_id, &skill.name, checked);
                        }
                        ui.label(RichText::new(&skill.name).size(theme::FONT_BODY).color(
                            if is_enabled { theme::TEXT } else { theme::TEXT_DIM }
                        ));
                        ui.colored_label(theme::TEXT_DIM, RichText::new(&skill.category).size(theme::FONT_CAPTION));
                    });
                }
            });
        }

        // ── 危險操作（兩次點擊確認）─────────────────────────────────────
        ui.add_space(theme::SP_XL);
        theme::thin_separator(ui);
        ui.add_space(theme::SP_SM);
        if !state.delete_confirming {
            if ui.add(egui::Button::new(RichText::new("刪除此 Agent").size(theme::FONT_SMALL).color(theme::DANGER))
                .fill(theme::DANGER.linear_multiply(0.08)).corner_radius(4.0)).clicked() {
                state.delete_confirming = true;
            }
        } else {
            ui.horizontal(|ui| {
                ui.colored_label(theme::DANGER, RichText::new("⚠ 確認刪除？此操作無法復原").size(theme::FONT_SMALL));
                if ui.add(egui::Button::new(RichText::new("確認刪除").size(theme::FONT_SMALL).color(theme::VALUE))
                    .fill(theme::DANGER).corner_radius(4.0)).clicked() {
                    svc.delete_agent(agent_id);
                    state.delete_confirming = false;
                }
                if ui.add(egui::Button::new(RichText::new("取消").size(theme::FONT_SMALL).color(theme::TEXT_DIM))
                    .fill(theme::CARD).corner_radius(4.0)).clicked() {
                    state.delete_confirming = false;
                }
            });
        }
    });
}

/// A setting row: fixed 100px label + inline widget/value. No right-stretch.
fn setting_row(ui: &mut egui::Ui, label: &str, widget: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(100.0, ui.spacing().interact_size.y),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| { ui.colored_label(theme::TEXT_DIM, RichText::new(label).size(theme::FONT_BODY)); },
        );
        widget(ui);
    });
}
