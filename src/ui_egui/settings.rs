//! Settings — GLOBAL system configuration.
//! Per-agent config lives in Workspace → ⚙ 設定 tab.
//! Typography: uses theme::FONT_* consistently.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::theme;
use crate::ui_service::*;

#[derive(Default)]
pub struct SettingsState {
    tg_code: String,
    tg_password: String,
    mcp_expanded: Option<String>,
    mcp_args: String,
    mcp_result: String,
    new_agent_id: String,
    new_agent_name: String,
    research_topic: String,
    research_url: String,
    persona_name_buf: String,
    persona_voice_buf: String,
    persona_obj_new: String,
    persona_loaded: bool,
    // Config export/import
    config_export: String,
    config_import: String,
    // Skill execution
    skill_test_id: String,
    skill_test_input: String,
    skill_test_output: String,


}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, _agents: &[AgentSummary], state: &mut SettingsState) {
    ScrollArea::vertical().id_salt("system_settings").show(ui, |ui| {
        ui.set_max_width(560.0);
        let s = svc.system_status();

        // ── Connection ───────────────────────────────────────────────────
        theme::section(ui, "連線狀態", |ui| {
            theme::status_row(ui, "Telegram", &s.telegram_status, s.telegram_connected);
            theme::status_row(ui, "RPC/MCP", if s.rpc_running { "Running (port 7700)" } else { "Stopped" }, s.rpc_running);

            if s.telegram_status.contains("CodeRequired") {
                ui.add_space(theme::SP_SM);
                ui.colored_label(theme::YELLOW, RichText::new("需要 Telegram 驗證碼：").size(theme::FONT_SMALL));
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut state.tg_code);
                    if ui.button("提交").clicked() && !state.tg_code.is_empty() {
                        svc.tg_submit_code(&state.tg_code); state.tg_code.clear();
                    }
                });
            }
            if s.telegram_status.contains("PasswordRequired") {
                ui.add_space(theme::SP_SM);
                ui.colored_label(theme::YELLOW, RichText::new("需要 2FA 密碼：").size(theme::FONT_SMALL));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut state.tg_password).password(true));
                    if ui.button("提交").clicked() && !state.tg_password.is_empty() {
                        svc.tg_submit_password(&state.tg_password); state.tg_password.clear();
                    }
                });
            }
            if !s.telegram_connected {
                ui.add_space(theme::SP_SM);
                if ui.button("🔄 重新連線 Telegram").clicked() { svc.tg_reconnect(); }
            }

            ui.add_space(theme::SP_MD);

            // Teams
            let teams_running = svc.teams_running();
            theme::status_row(ui, "Teams", if teams_running { "監聽中" } else { "未啟動" }, teams_running);
            if !teams_running {
                ui.add_space(theme::SP_SM);
                ui.colored_label(theme::TEXT_DIM, RichText::new("需要 Chrome 瀏覽器").size(theme::FONT_CAPTION));
                if ui.add(egui::Button::new(RichText::new("🚀 啟動 Teams").size(theme::FONT_SMALL).color(theme::BG))
                    .fill(theme::INFO).corner_radius(4.0)).clicked() {
                    svc.start_teams();
                }
            }
        });

        // ── LLM ──────────────────────────────────────────────────────────
        theme::section(ui, "LLM 模型", |ui| {
            theme::info_row(ui, "主模型", &format!("{} ({})", s.llm.main_model, s.llm.main_backend));
            theme::info_row(ui, "Router", &format!("{} ({})", s.llm.router_model, s.llm.router_backend));
            theme::info_row(ui, "遠端", if s.llm.is_remote { "是" } else { "否（本地）" });
        });

        // ── MCP ──────────────────────────────────────────────────────────
        let mcp_tools = svc.mcp_tools();
        theme::section(ui, &format!("MCP 工具 ({})", mcp_tools.len()), |ui| {
            if mcp_tools.is_empty() {
                ui.colored_label(theme::TEXT_DIM, RichText::new("未連接 — 編輯 config/mcp_servers.yaml").size(theme::FONT_SMALL));
                return;
            }
            for tool in &mcp_tools {
                let is_expanded = state.mcp_expanded.as_deref() == Some(&tool.registry_name);
                ui.horizontal(|ui| {
                    let arrow = if is_expanded { "▼" } else { "▶" };
                    if ui.add(egui::Button::new(RichText::new(arrow).size(theme::FONT_CAPTION).color(theme::TEXT_DIM))
                        .fill(egui::Color32::TRANSPARENT)).clicked() {
                        state.mcp_expanded = if is_expanded { None } else {
                            state.mcp_args = "{}".to_string();
                            state.mcp_result.clear();
                            Some(tool.registry_name.clone())
                        };
                    }
                    ui.colored_label(theme::INFO, RichText::new(&tool.tool_name).size(theme::FONT_BODY));
                    ui.colored_label(theme::TEXT_DIM, RichText::new(&tool.description).size(theme::FONT_CAPTION));
                });
                if is_expanded {
                    ui.indent("mcp_detail", |ui| {
                        if !tool.params.is_empty() {
                            ui.horizontal(|ui| {
                                for (name, typ) in &tool.params {
                                    theme::badge(ui, &format!("{name}: {typ}"), theme::INFO);
                                }
                            });
                        }
                        ui.horizontal(|ui| {
                            ui.add_sized([ui.available_width() - 56.0, 24.0],
                                egui::TextEdit::singleline(&mut state.mcp_args).font(egui::TextStyle::Monospace));
                            if ui.add(egui::Button::new(RichText::new("▶ 執行").size(theme::FONT_SMALL).color(theme::BG))
                                .fill(theme::INFO).corner_radius(4.0)).clicked() {
                                state.mcp_result = svc.mcp_call(&tool.registry_name, &state.mcp_args).unwrap_or_else(|e| format!("❌ {e}"));
                            }
                        });
                        if !state.mcp_result.is_empty() {
                            ui.add_space(theme::SP_XS);
                            egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM).show(ui, |ui| {
                                ui.colored_label(theme::ACCENT, RichText::new(&state.mcp_result).size(theme::FONT_SMALL).monospace());
                            });
                        }
                    });
                    ui.add_space(theme::SP_SM);
                }
            }
        });

        // ── Skills ───────────────────────────────────────────────────────
        theme::section(ui, &format!("技能 ({})", s.skills.len()), |ui| {
            for skill in &s.skills {
                ui.horizontal(|ui| {
                    theme::badge(ui, &skill.category, theme::INFO);
                    ui.label(RichText::new(&skill.name).size(theme::FONT_BODY).color(theme::TEXT));
                });
            }
        });

        // ── LLM Model Selection ─────────────────────────────────────────
        theme::section(ui, "模型選擇", |ui| {
            let models = svc.available_models();
            if models.is_empty() {
                ui.colored_label(theme::TEXT_DIM, RichText::new("無可用模型").size(theme::FONT_SMALL));
            } else {
                for model in &models {
                    let is_current = model == &s.llm.main_model;
                    ui.horizontal(|ui| {
                        if is_current {
                            ui.colored_label(theme::ACCENT, "●");
                        } else {
                            if ui.add(egui::Label::new(RichText::new("○").color(theme::TEXT_DIM))
                                .selectable(false).sense(egui::Sense::click())).clicked() {
                                svc.set_main_model(model);
                            }
                        }
                        ui.label(RichText::new(model).size(theme::FONT_BODY).color(
                            if is_current { theme::TEXT } else { theme::TEXT_DIM }
                        ).family(egui::FontFamily::Monospace));
                    });
                }
            }
        });

        // ── Persona (full edit) ──────────────────────────────────────────
        theme::section(ui, "全局人格", |ui| {
            if !state.persona_loaded {
                state.persona_name_buf = svc.persona_name();
                state.persona_voice_buf = svc.persona_voice();
                state.persona_loaded = true;
            }
            // Name
            theme::info_row(ui, "名稱", "");
            let resp = ui.add_sized([250.0, 24.0], egui::TextEdit::singleline(&mut state.persona_name_buf));
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                svc.set_persona_name(&state.persona_name_buf);
            }
            // Voice
            theme::info_row(ui, "語氣風格", "");
            let resp2 = ui.add_sized([250.0, 24.0], egui::TextEdit::singleline(&mut state.persona_voice_buf));
            if resp2.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                svc.set_persona_voice(&state.persona_voice_buf);
            }
            // Objectives
            ui.add_space(theme::SP_SM);
            ui.colored_label(theme::TEXT_DIM, RichText::new("全局目標:").size(theme::FONT_SMALL));
            let objectives = svc.persona_objectives();
            let mut remove_idx: Option<usize> = None;
            for (i, obj) in objectives.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.colored_label(theme::ACCENT, "•");
                    ui.label(RichText::new(obj).size(theme::FONT_BODY).color(theme::TEXT));
                    let del = ui.add(egui::Label::new(RichText::new("  x").size(theme::FONT_BODY).color(theme::DANGER.linear_multiply(0.6)))
                        .selectable(false).sense(egui::Sense::click()));
                    if del.clicked() { remove_idx = Some(i); }
                });
            }
            if let Some(idx) = remove_idx {
                let mut objs = objectives.clone();
                objs.remove(idx);
                svc.set_persona_objectives(objs);
            }
            ui.horizontal(|ui| {
                ui.add_sized([200.0, 24.0], egui::TextEdit::singleline(&mut state.persona_obj_new).hint_text("新增目標..."));
                if ui.add(egui::Button::new(RichText::new("+").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked()
                    && !state.persona_obj_new.trim().is_empty() {
                    let mut objs = svc.persona_objectives();
                    objs.push(state.persona_obj_new.trim().to_string());
                    svc.set_persona_objectives(objs);
                    state.persona_obj_new.clear();
                }
            });
        });

        // ── New Agent ────────────────────────────────────────────────────
        theme::section(ui, "新增 Agent", |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(theme::TEXT_DIM, RichText::new("ID").size(theme::FONT_SMALL));
                ui.add_sized([120.0, 24.0], egui::TextEdit::singleline(&mut state.new_agent_id).hint_text("agent_id"));
                ui.colored_label(theme::TEXT_DIM, RichText::new("名稱").size(theme::FONT_SMALL));
                ui.add_sized([120.0, 24.0], egui::TextEdit::singleline(&mut state.new_agent_name).hint_text("顯示名稱"));
                let can = !state.new_agent_id.trim().is_empty() && !state.new_agent_name.trim().is_empty();
                if ui.add_enabled(can, egui::Button::new(RichText::new("+ 建立").size(theme::FONT_SMALL).color(theme::BG))
                    .fill(theme::ACCENT).corner_radius(4.0)).clicked() {
                    svc.create_agent(state.new_agent_id.trim(), state.new_agent_name.trim());
                    state.new_agent_id.clear();
                    state.new_agent_name.clear();
                }
            });
        });

        // ── Research ─────────────────────────────────────────────────────
        theme::section(ui, "觸發調研", |ui| {
            ui.horizontal(|ui| {
                ui.add_sized([200.0, 24.0], egui::TextEdit::singleline(&mut state.research_topic).hint_text("主題..."));
                ui.add_sized([150.0, 24.0], egui::TextEdit::singleline(&mut state.research_url).hint_text("URL（選填）"));
                if ui.add(egui::Button::new(RichText::new("🔍").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked()
                    && !state.research_topic.trim().is_empty() {
                    let url = if state.research_url.trim().is_empty() { None } else { Some(state.research_url.trim()) };
                    svc.trigger_research(state.research_topic.trim(), url);
                    state.research_topic.clear();
                    state.research_url.clear();
                }
            });
        });

        // ── Skill Execution ──────────────────────────────────────────────
        theme::section(ui, "技能測試", |ui| {
            ui.horizontal(|ui| {
                ui.add_sized([150.0, 24.0], egui::TextEdit::singleline(&mut state.skill_test_id).hint_text("skill_id"));
                ui.add_sized([200.0, 24.0], egui::TextEdit::singleline(&mut state.skill_test_input).hint_text("輸入..."));
                if ui.add(egui::Button::new(RichText::new("▶").color(theme::BG)).fill(theme::ACCENT).corner_radius(4.0)).clicked()
                    && !state.skill_test_id.trim().is_empty() {
                    state.skill_test_output = svc.execute_skill(state.skill_test_id.trim(), state.skill_test_input.trim());
                }
            });
            if !state.skill_test_output.is_empty() {
                egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM).show(ui, |ui| {
                    ui.colored_label(theme::ACCENT, RichText::new(&state.skill_test_output).size(theme::FONT_SMALL).monospace());
                });
            }
        });

        // ── Config Export/Import ─────────────────────────────────────────
        theme::section(ui, "設定備份", |ui| {
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("匯出").size(theme::FONT_SMALL).color(theme::BG)).fill(theme::INFO).corner_radius(4.0)).clicked() {
                    state.config_export = svc.export_config();
                }
                if ui.add(egui::Button::new(RichText::new("匯入").size(theme::FONT_SMALL).color(theme::BG)).fill(theme::YELLOW).corner_radius(4.0)).clicked()
                    && !state.config_import.trim().is_empty() {
                    if let Err(e) = svc.import_config(&state.config_import) {
                        state.config_export = e;
                    }
                }
            });
            if !state.config_export.is_empty() {
                ui.add_sized([ui.available_width(), 80.0], egui::TextEdit::multiline(&mut state.config_export).font(egui::TextStyle::Monospace));
            }
            ui.colored_label(theme::TEXT_DIM, RichText::new("貼上 YAML 後點「匯入」:").size(theme::FONT_CAPTION));
            ui.add_sized([ui.available_width(), 60.0], egui::TextEdit::multiline(&mut state.config_import).font(egui::TextStyle::Monospace).hint_text("貼上 agents.yaml 內容..."));
        });

        // ── Notification History ─────────────────────────────────────────
        theme::section(ui, "通知歷史", |ui| {
            let history = svc.toast_history();
            if history.is_empty() {
                ui.colored_label(theme::TEXT_DIM, "暫無通知");
            } else {
                for evt in history.iter().rev().take(20) {
                    let (icon, color) = match evt.level {
                        ToastLevel::Success => ("✓", theme::ACCENT),
                        ToastLevel::Error => ("✗", theme::DANGER),
                        ToastLevel::Info => ("ℹ", theme::INFO),
                    };
                    ui.horizontal(|ui| {
                        ui.colored_label(color, RichText::new(icon).size(theme::FONT_SMALL));
                        ui.colored_label(color, RichText::new(&evt.text).size(theme::FONT_SMALL));
                    });
                }
            }
        });
    });
}
