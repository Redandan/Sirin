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
    // New agent
    new_agent_id: String,
    new_agent_name: String,
    // Research
    research_topic: String,
    research_url: String,
    // Persona
    persona_name_buf: String,
    persona_loaded: bool,
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

        // ── Persona ──────────────────────────────────────────────────────
        theme::section(ui, "全局人格", |ui| {
            if !state.persona_loaded {
                state.persona_name_buf = svc.persona_name();
                state.persona_loaded = true;
            }
            ui.horizontal(|ui| {
                ui.colored_label(theme::TEXT_DIM, RichText::new("名稱").size(theme::FONT_BODY));
                ui.add_space(theme::SP_SM);
                let resp = ui.add_sized([200.0, 24.0], egui::TextEdit::singleline(&mut state.persona_name_buf));
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    svc.set_persona_name(&state.persona_name_buf);
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

        // ── Research Trigger ─────────────────────────────────────────────
        theme::section(ui, "觸發調研", |ui| {
            ui.horizontal(|ui| {
                ui.add_sized([200.0, 24.0], egui::TextEdit::singleline(&mut state.research_topic).hint_text("調研主題..."));
                ui.add_sized([150.0, 24.0], egui::TextEdit::singleline(&mut state.research_url).hint_text("URL（選填）"));
                if ui.add(egui::Button::new(RichText::new("🔍 開始").size(theme::FONT_SMALL).color(theme::BG))
                    .fill(theme::ACCENT).corner_radius(4.0)).clicked()
                    && !state.research_topic.trim().is_empty()
                {
                    let url = if state.research_url.trim().is_empty() { None } else { Some(state.research_url.trim()) };
                    svc.trigger_research(state.research_topic.trim(), url);
                    state.research_topic.clear();
                    state.research_url.clear();
                }
            });
        });
    });
}
