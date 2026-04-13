//! Settings — agent detail + system status.
//!
//! AI reads this: left list of agents + "System" button, right panel shows
//! agent config (identity, behavior, KPI) or system status (LLM, TG, MCP, skills).

use std::sync::Arc;

use eframe::egui::{self, Color32, RichText, ScrollArea};

use crate::ui_service::*;

#[derive(Default)]
pub struct SettingsState {
    selected: usize, // 0..N = agent, usize::MAX = system
}

pub fn show(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    agents: &[AgentSummary],
    state: &mut SettingsState,
) {
    ui.horizontal(|ui| {
        // Left: agent list
        egui::Frame::new()
            .fill(Color32::from_rgb(24, 26, 30))
            .show(ui, |ui| {
                ui.set_width(160.0);
                ui.label(RichText::new("Agent 列表").strong().small().color(Color32::GRAY));
                ui.separator();
                for (idx, agent) in agents.iter().enumerate() {
                    if ui.selectable_label(state.selected == idx, &agent.name).clicked() {
                        state.selected = idx;
                    }
                }
                ui.separator();
                if ui.selectable_label(state.selected == usize::MAX, "⚙ 系統").clicked() {
                    state.selected = usize::MAX;
                }
            });

        ui.separator();

        // Right: detail
        ScrollArea::vertical().id_salt("settings_detail").show(ui, |ui| {
            if state.selected == usize::MAX {
                show_system(ui, svc);
            } else if let Some(agent) = agents.get(state.selected) {
                if let Some(detail) = svc.agent_detail(&agent.id) {
                    show_agent_detail(ui, &detail);
                }
            }
        });
    });
}

fn show_agent_detail(ui: &mut egui::Ui, d: &AgentDetailView) {
    ui.heading(&d.name);
    ui.horizontal(|ui| {
        let (label, color) = if d.enabled {
            ("啟用", Color32::from_rgb(80, 200, 100))
        } else {
            ("停用", Color32::GRAY)
        };
        ui.colored_label(color, label);
        ui.colored_label(Color32::GRAY, format!("| {} | {}", d.platform, d.professional_tone));
    });
    ui.add_space(8.0);

    // Objectives
    section(ui, "目標", |ui| {
        if d.objectives.is_empty() {
            ui.colored_label(Color32::DARK_GRAY, "（使用全域 Persona 目標）");
        }
        for obj in &d.objectives {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(100, 180, 255), "•");
                ui.label(obj);
            });
        }
    });

    // Human behavior
    section(ui, "人性化行為", |ui| {
        info_row(ui, "啟用", &format!("{}", d.human_behavior_enabled));
        info_row(ui, "延遲範圍", &format!("{}–{}s", d.min_reply_delay, d.max_reply_delay));
        info_row(ui, "每小時上限", &format!("{}", d.max_per_hour));
        info_row(ui, "每日上限", &format!("{}", d.max_per_day));
    });

    // KPI
    if !d.kpi_labels.is_empty() {
        section(ui, "KPI", |ui| {
            for (label, unit) in &d.kpi_labels {
                info_row(ui, label, unit);
            }
        });
    }
}

fn show_system(ui: &mut egui::Ui, svc: &Arc<dyn AppService>) {
    let s = svc.system_status();

    ui.heading("系統設定");
    ui.add_space(8.0);

    section(ui, "連線狀態", |ui| {
        status_row(ui, "Telegram", &s.telegram_status, s.telegram_connected);
        status_row(ui, "RPC/MCP", if s.rpc_running { "Running (7700)" } else { "Stopped" }, s.rpc_running);
    });

    section(ui, "LLM 配置", |ui| {
        info_row(ui, "主模型", &format!("{} ({})", s.llm.main_model, s.llm.main_backend));
        info_row(ui, "Router", &format!("{} ({})", s.llm.router_model, s.llm.router_backend));
        info_row(ui, "遠端", if s.llm.is_remote { "是" } else { "否（本地）" });
    });

    section(ui, "MCP 外部工具", |ui| {
        if s.mcp_tools.is_empty() {
            ui.colored_label(Color32::DARK_GRAY, "未連接");
        }
        for tool in &s.mcp_tools {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(100, 180, 255), &tool.name);
                ui.colored_label(Color32::GRAY, RichText::new(&tool.description).small());
            });
        }
    });

    section(ui, "技能列表", |ui| {
        for skill in &s.skills {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::GRAY, RichText::new(&skill.category).small());
                ui.label(&skill.name);
            });
        }
    });
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn section(ui: &mut egui::Ui, title: &str, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(Color32::from_rgb(28, 32, 38))
        .corner_radius(6.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.label(RichText::new(title).strong().small().color(Color32::GRAY));
            ui.add_space(4.0);
            content(ui);
        });
    ui.add_space(6.0);
}

fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.colored_label(Color32::GRAY, RichText::new(label).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).small());
        });
    });
}

fn status_row(ui: &mut egui::Ui, label: &str, status: &str, ok: bool) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (dot, color) = if ok { ("●", Color32::from_rgb(80, 200, 100)) } else { ("○", Color32::from_rgb(220, 80, 80)) };
            ui.colored_label(color, format!("{dot} {status}"));
        });
    });
}
