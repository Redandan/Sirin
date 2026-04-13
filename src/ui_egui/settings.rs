//! Settings — GLOBAL system configuration only.
//!
//! Per-agent config lives in Workspace → ⚙ 設定 tab.
//! This page handles: LLM, Telegram auth, MCP tools, Skills.

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
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, _agents: &[AgentSummary], state: &mut SettingsState) {
    ScrollArea::vertical().id_salt("system_settings").show(ui, |ui| {
        let s = svc.system_status();

        // ── Connection & Telegram auth ───────────────────────────────────
        theme::section(ui, "連線狀態", |ui| {
            theme::status_row(ui, "Telegram", &s.telegram_status, s.telegram_connected);
            theme::status_row(ui, "RPC/MCP Server", if s.rpc_running { "Running (port 7700)" } else { "Stopped" }, s.rpc_running);

            if s.telegram_status.contains("CodeRequired") {
                ui.add_space(theme::GAP_SM);
                ui.colored_label(theme::YELLOW, "需要 Telegram 驗證碼：");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut state.tg_code);
                    if ui.button("提交").clicked() && !state.tg_code.is_empty() {
                        svc.tg_submit_code(&state.tg_code); state.tg_code.clear();
                    }
                });
            }
            if s.telegram_status.contains("PasswordRequired") {
                ui.add_space(theme::GAP_SM);
                ui.colored_label(theme::YELLOW, "需要 2FA 密碼：");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut state.tg_password).password(true));
                    if ui.button("提交").clicked() && !state.tg_password.is_empty() {
                        svc.tg_submit_password(&state.tg_password); state.tg_password.clear();
                    }
                });
            }
            if !s.telegram_connected {
                ui.add_space(theme::GAP_SM);
                if ui.button("🔄 重新連線 Telegram").clicked() { svc.tg_reconnect(); }
            }
        });

        // ── LLM ─────────────────────────────────────────────────────────
        theme::section(ui, "LLM 模型配置", |ui| {
            theme::info_row(ui, "主模型", &format!("{} ({})", s.llm.main_model, s.llm.main_backend));
            theme::info_row(ui, "Router 模型", &format!("{} ({})", s.llm.router_model, s.llm.router_backend));
            theme::info_row(ui, "遠端模式", if s.llm.is_remote { "是" } else { "否（本地）" });
        });

        // ── MCP Tools (interactive) ──────────────────────────────────────
        let mcp_tools = svc.mcp_tools();
        theme::section(ui, &format!("MCP 外部工具 ({})", mcp_tools.len()), |ui| {
            if mcp_tools.is_empty() {
                ui.colored_label(theme::OVERLAY0, "未連接 — 編輯 config/mcp_servers.yaml 並重啟");
                return;
            }
            for tool in &mcp_tools {
                let is_expanded = state.mcp_expanded.as_deref() == Some(&tool.registry_name);
                ui.horizontal(|ui| {
                    let arrow = if is_expanded { "▼" } else { "▶" };
                    if ui.small_button(arrow).clicked() {
                        state.mcp_expanded = if is_expanded { None } else {
                            state.mcp_args = "{}".to_string();
                            state.mcp_result.clear();
                            Some(tool.registry_name.clone())
                        };
                    }
                    ui.colored_label(theme::BLUE, &tool.tool_name);
                    ui.colored_label(theme::OVERLAY0, RichText::new(&tool.description).small());
                });
                if is_expanded {
                    ui.indent("mcp_detail", |ui| {
                        if !tool.params.is_empty() {
                            ui.horizontal(|ui| {
                                for (name, typ) in &tool.params {
                                    theme::badge(ui, &format!("{name}: {typ}"), theme::MAUVE);
                                }
                            });
                        }
                        ui.horizontal(|ui| {
                            ui.add_sized([ui.available_width() - 60.0, 24.0],
                                egui::TextEdit::singleline(&mut state.mcp_args).font(egui::TextStyle::Monospace));
                            if ui.add(egui::Button::new(RichText::new("▶ 執行").color(theme::CRUST)).fill(theme::BLUE).corner_radius(4.0)).clicked() {
                                state.mcp_result = svc.mcp_call(&tool.registry_name, &state.mcp_args).unwrap_or_else(|e| format!("❌ {e}"));
                            }
                        });
                        if !state.mcp_result.is_empty() {
                            egui::Frame::new().fill(theme::CRUST).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
                                ui.colored_label(theme::TEAL, RichText::new(&state.mcp_result).monospace().small());
                            });
                        }
                    });
                    ui.add_space(theme::GAP_SM);
                }
            }
        });

        // ── Skills ───────────────────────────────────────────────────────
        theme::section(ui, &format!("技能列表 ({})", s.skills.len()), |ui| {
            for skill in &s.skills {
                ui.horizontal(|ui| {
                    theme::badge(ui, &skill.category, theme::MAUVE);
                    ui.label(RichText::new(&skill.name).color(theme::TEXT));
                    ui.colored_label(theme::OVERLAY0, RichText::new(&skill.description).small());
                });
            }
        });
    });
}
