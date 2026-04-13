//! Settings — editable agent detail + system panel with TG auth.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::theme;
use crate::ui_service::*;

#[derive(Default)]
pub struct SettingsState {
    selected: usize,
    new_objective: String,
    tg_code: String,
    tg_password: String,
    /// MCP tool test: which tool is expanded for testing.
    mcp_expanded: Option<String>,
    mcp_args: String,
    mcp_result: String,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, agents: &[AgentSummary], state: &mut SettingsState) {
    ui.horizontal(|ui| {
        egui::Frame::new().fill(theme::MANTLE).show(ui, |ui| {
            ui.set_width(160.0);
            ui.label(RichText::new("Agent 列表").strong().small().color(theme::OVERLAY0));
            ui.separator();
            for (idx, agent) in agents.iter().enumerate() {
                let sel = ui.selectable_label(state.selected == idx, RichText::new(&agent.name).color(theme::TEXT));
                if sel.clicked() { state.selected = idx; }
            }
            ui.separator();
            if ui.selectable_label(state.selected == usize::MAX, RichText::new("⚙ 系統").color(theme::TEXT)).clicked() {
                state.selected = usize::MAX;
            }
        });
        ui.separator();

        ScrollArea::vertical().id_salt("settings_detail").show(ui, |ui| {
            if state.selected == usize::MAX { show_system(ui, svc, state); }
            else if let Some(agent) = agents.get(state.selected) {
                if let Some(d) = svc.agent_detail(&agent.id) { show_agent(ui, svc, &d, state); }
            }
        });
    });
}

fn show_agent(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, d: &AgentDetailView, state: &mut SettingsState) {
    // Compact header with inline controls
    ui.horizontal(|ui| {
        ui.label(RichText::new(&d.name).strong().size(16.0).color(theme::TEXT));
    });
    ui.horizontal(|ui| {
        let mut enabled = d.enabled;
        if ui.checkbox(&mut enabled, "").changed() { svc.toggle_agent(&d.id, enabled); }
        theme::badge(ui, if d.enabled { "啟用" } else { "停用" }, if d.enabled { theme::GREEN } else { theme::OVERLAY0 });
        ui.colored_label(theme::OVERLAY0, format!("| {} | {}", d.platform, d.professional_tone));
    });
    ui.add_space(theme::GAP_MD);

    // Objectives (editable)
    theme::section(ui, "目標", |ui| {
        if d.objectives.is_empty() { ui.colored_label(theme::OVERLAY0, "（使用全域 Persona 目標）"); }
        let mut remove_idx: Option<usize> = None;
        for (i, obj) in d.objectives.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.colored_label(theme::BLUE, "•");
                ui.label(RichText::new(obj).color(theme::TEXT));
                if ui.small_button("✗").clicked() { remove_idx = Some(i); }
            });
        }
        if let Some(idx) = remove_idx { svc.remove_objective(&d.id, idx); }
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut state.new_objective);
            if ui.button("+ 新增").clicked() && !state.new_objective.trim().is_empty() {
                svc.add_objective(&d.id, state.new_objective.trim());
                state.new_objective.clear();
            }
        });
    });

    theme::section(ui, "人性化行為", |ui| {
        theme::info_row(ui, "啟用", &format!("{}", d.human_behavior_enabled));
        theme::info_row(ui, "延遲範圍", &format!("{}–{}s", d.min_reply_delay, d.max_reply_delay));
        theme::info_row(ui, "每小時上限", &format!("{}", d.max_per_hour));
        theme::info_row(ui, "每日上限", &format!("{}", d.max_per_day));
    });

    if !d.kpi_labels.is_empty() {
        theme::section(ui, "KPI", |ui| {
            for (label, unit) in &d.kpi_labels { theme::info_row(ui, label, unit); }
        });
    }
}

fn show_system(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut SettingsState) {
    let s = svc.system_status();
    // No heading needed — top bar shows "設定 / Agent 配置 & 系統"
    ui.add_space(theme::GAP_SM);

    theme::section(ui, "連線狀態", |ui| {
        theme::status_row(ui, "Telegram", &s.telegram_status, s.telegram_connected);
        theme::status_row(ui, "RPC/MCP", if s.rpc_running { "Running (7700)" } else { "Stopped" }, s.rpc_running);
        if s.telegram_status.contains("CodeRequired") {
            ui.add_space(theme::GAP_SM);
            ui.colored_label(theme::YELLOW, "需要驗證碼：");
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
            if ui.button("🔄 重新連線").clicked() { svc.tg_reconnect(); }
        }
    });

    theme::section(ui, "LLM 配置", |ui| {
        theme::info_row(ui, "主模型", &format!("{} ({})", s.llm.main_model, s.llm.main_backend));
        theme::info_row(ui, "Router", &format!("{} ({})", s.llm.router_model, s.llm.router_backend));
        theme::info_row(ui, "遠端", if s.llm.is_remote { "是" } else { "否（本地）" });
    });

    // MCP tools with interactive execution
    let mcp_tools = svc.mcp_tools();
    theme::section(ui, &format!("MCP 外部工具 ({})", mcp_tools.len()), |ui| {
        if mcp_tools.is_empty() {
            ui.colored_label(theme::OVERLAY0, "未連接 — 配置 config/mcp_servers.yaml");
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
                    // Show params
                    if !tool.params.is_empty() {
                        ui.horizontal(|ui| {
                            ui.colored_label(theme::OVERLAY0, "參數:");
                            for (name, typ) in &tool.params {
                                theme::badge(ui, &format!("{name}: {typ}"), theme::MAUVE);
                            }
                        });
                    }
                    // Args input + run button
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("JSON:").small().color(theme::OVERLAY0));
                        ui.add_sized([ui.available_width() - 60.0, 24.0],
                            egui::TextEdit::singleline(&mut state.mcp_args).font(egui::TextStyle::Monospace));
                        if ui.add(egui::Button::new(RichText::new("▶ 執行").color(theme::CRUST)).fill(theme::BLUE).corner_radius(4.0)).clicked() {
                            state.mcp_result = match svc.mcp_call(&tool.registry_name, &state.mcp_args) {
                                Ok(r) => r,
                                Err(e) => format!("❌ {e}"),
                            };
                        }
                    });
                    // Result
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

    theme::section(ui, "技能列表", |ui| {
        for skill in &s.skills {
            ui.horizontal(|ui| {
                theme::badge(ui, &skill.category, theme::MAUVE);
                ui.label(RichText::new(&skill.name).color(theme::TEXT));
            });
        }
    });
}
