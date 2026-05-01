//! OPS Panel — three sub-tabs:
//!   [AI Router]      route_query / benchmark_llms / manage intents
//!   [Session & Tasks] save_point / tasks / handoff history
//!   [Cost & KB]      session_cost leaderboard / kb_stats

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use crate::ui_service::AppService;
use super::theme;

// ── Tabs ──────────────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum OpsTab { #[default] AiRouter, SessionTasks, CostKb }

// ── State ─────────────────────────────────────────────────────────────────────

pub struct OpsPanelState {
    pub tab: OpsTab,
    // AI Router
    ai_intent:   String,
    ai_prompt:   String,
    ai_result:   String,
    ai_running:  bool,
    bench_prompt: String,
    bench_result: String,
    intents_raw: String,     // cached JSON display
    new_intent_name:    String,
    new_intent_backend: String,
    // Session & Tasks
    tasks_raw:   String,
    task_desc:   String,
    task_proj:   String,
    task_prio:   String,
    points_raw:  String,
    point_label: String,
    point_summary: String,
    // Cost & KB
    cost_raw:    String,
    kb_stats_raw: String,
    cost_project: String,
}

impl Default for OpsPanelState {
    fn default() -> Self {
        Self {
            tab: OpsTab::default(),
            ai_intent:   "code-review".to_string(),
            ai_prompt:   String::new(),
            ai_result:   String::new(),
            ai_running:  false,
            bench_prompt: String::new(),
            bench_result: String::new(),
            intents_raw: String::new(),
            new_intent_name:    String::new(),
            new_intent_backend: "gemini".to_string(),
            tasks_raw:   String::new(),
            task_desc:   String::new(),
            task_proj:   "sirin".to_string(),
            task_prio:   "P1".to_string(),
            points_raw:  String::new(),
            point_label: String::new(),
            point_summary: String::new(),
            cost_raw:    String::new(),
            kb_stats_raw: String::new(),
            cost_project: "sirin".to_string(),
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut OpsPanelState) {
    // Tab bar.
    ui.horizontal(|ui| {
        tab_btn(ui, "AI ROUTER",       OpsTab::AiRouter,      &mut state.tab);
        ui.add_space(theme::SP_MD);
        tab_btn(ui, "SESSION & TASKS", OpsTab::SessionTasks,  &mut state.tab);
        ui.add_space(theme::SP_MD);
        tab_btn(ui, "COST & KB",       OpsTab::CostKb,        &mut state.tab);
    });
    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    match state.tab {
        OpsTab::AiRouter     => show_ai_router(ui, svc, state),
        OpsTab::SessionTasks => show_session_tasks(ui, svc, state),
        OpsTab::CostKb       => show_cost_kb(ui, svc, state),
    }
}

fn tab_btn(ui: &mut egui::Ui, label: &str, tab: OpsTab, active: &mut OpsTab) {
    let selected = *active == tab;
    let text = RichText::new(label).size(theme::FONT_SMALL).strong();
    let text = if selected { text.color(theme::ACCENT) } else { text.color(theme::TEXT_DIM) };
    if ui.add(egui::Button::new(text).frame(false)).clicked() { *active = tab; }
    if selected {
        let r = ui.min_rect();
        ui.painter().hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(2.0, theme::ACCENT));
    }
}

// ── AI Router tab ─────────────────────────────────────────────────────────────

fn show_ai_router(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut OpsPanelState) {
    ScrollArea::vertical().id_salt("ops_ai").show(ui, |ui| {

        // ── Route Query ────────────────────────────────────────────────
        theme::section_header(ui, "Route Query");
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("Intent").size(theme::FONT_SMALL));
            ui.add(egui::TextEdit::singleline(&mut state.ai_intent)
                .desired_width(140.0)
                .font(egui::TextStyle::Monospace)
                .hint_text("e.g. code-review"));
        });
        ui.add_space(theme::SP_XS);
        ui.add(egui::TextEdit::multiline(&mut state.ai_prompt)
            .desired_width(f32::INFINITY)
            .desired_rows(3)
            .hint_text("Enter prompt…")
            .font(egui::TextStyle::Body));
        ui.add_space(theme::SP_XS);
        ui.horizontal(|ui| {
            let btn = egui::Button::new(
                RichText::new("▶ Route Query").size(theme::FONT_SMALL).color(theme::BG)
            ).fill(theme::ACCENT).corner_radius(4.0);
            if ui.add_enabled(!state.ai_prompt.is_empty(), btn).clicked() {
                let args = format!(
                    r#"{{"intent":"{}","prompt":{}}}"#,
                    state.ai_intent,
                    serde_json::to_string(&state.ai_prompt).unwrap_or_default()
                );
                state.ai_result = svc.mcp_call("sirin/route_query", &args)
                    .unwrap_or_else(|e| format!("❌ {e}"));
            }
            ui.add_space(theme::SP_SM);
            let bench_btn = egui::Button::new(
                RichText::new("⚡ Benchmark LLMs").size(theme::FONT_SMALL).color(theme::BG)
            ).fill(theme::INFO).corner_radius(4.0);
            if ui.add_enabled(!state.ai_prompt.is_empty(), bench_btn).clicked() {
                let args = format!(
                    r#"{{"prompt":{},"backends":"gemini,deepseek"}}"#,
                    serde_json::to_string(&state.ai_prompt).unwrap_or_default()
                );
                state.ai_result = svc.mcp_call("sirin/benchmark_llms", &args)
                    .unwrap_or_else(|e| format!("❌ {e}"));
            }
        });
        if !state.ai_result.is_empty() {
            ui.add_space(theme::SP_XS);
            egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM)
                .show(ui, |ui| {
                    ScrollArea::vertical().id_salt("ai_result").max_height(160.0).show(ui, |ui| {
                        ui.colored_label(theme::ACCENT,
                            RichText::new(&state.ai_result).size(theme::FONT_SMALL).monospace());
                    });
                });
        }

        ui.add_space(theme::SP_LG);

        // ── Intent Registry ────────────────────────────────────────────
        theme::section_header(ui, "Intent Registry");
        ui.horizontal(|ui| {
            if ui.button(RichText::new("↻ Load").size(theme::FONT_SMALL)).clicked() {
                state.intents_raw = svc.mcp_call("sirin/list_intents", "{}")
                    .unwrap_or_else(|e| format!("❌ {e}"));
            }
        });
        if !state.intents_raw.is_empty() {
            egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM)
                .show(ui, |ui| {
                    ui.colored_label(theme::TEXT_DIM,
                        RichText::new(&state.intents_raw).size(theme::FONT_CAPTION).monospace());
                });
        }
        ui.add_space(theme::SP_XS);
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("Name").size(theme::FONT_SMALL));
            ui.add(egui::TextEdit::singleline(&mut state.new_intent_name)
                .desired_width(120.0).font(egui::TextStyle::Monospace));
            ui.colored_label(theme::TEXT_DIM, RichText::new("Backend").size(theme::FONT_SMALL));
            egui::ComboBox::from_id_salt("intent_backend")
                .width(100.0)
                .selected_text(&state.new_intent_backend)
                .show_ui(ui, |ui| {
                    for b in ["gemini", "deepseek", "claude", "ollama"] {
                        ui.selectable_value(&mut state.new_intent_backend, b.to_string(), b);
                    }
                });
            if ui.add_enabled(
                !state.new_intent_name.is_empty(),
                egui::Button::new(RichText::new("+ Register").size(theme::FONT_SMALL))
            ).clicked() {
                let args = format!(
                    r#"{{"name":"{}","backend":"{}"}}"#,
                    state.new_intent_name, state.new_intent_backend
                );
                let r = svc.mcp_call("sirin/register_intent", &args)
                    .unwrap_or_else(|e| format!("❌ {e}"));
                state.intents_raw = r;
                state.new_intent_name.clear();
            }
        });
    });
}

// ── Session & Tasks tab ───────────────────────────────────────────────────────

fn show_session_tasks(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut OpsPanelState) {
    ScrollArea::vertical().id_salt("ops_session").show(ui, |ui| {

        // ── Tasks ───────────────────────────────────────────────────���──
        theme::section_header(ui, "Tasks");
        ui.horizontal(|ui| {
            if ui.button(RichText::new("↻ Load").size(theme::FONT_SMALL)).clicked() {
                state.tasks_raw = svc.mcp_call("sirin/list_tasks",
                    r#"{"status":"open"}"#)
                    .unwrap_or_else(|e| format!("❌ {e}"));
            }
        });
        if !state.tasks_raw.is_empty() {
            egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM)
                .show(ui, |ui| {
                    ScrollArea::vertical().id_salt("tasks_raw").max_height(120.0).show(ui, |ui| {
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new(&state.tasks_raw).size(theme::FONT_CAPTION).monospace());
                    });
                });
        }
        ui.add_space(theme::SP_XS);
        // Create task form.
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("Project").size(theme::FONT_SMALL));
            ui.add(egui::TextEdit::singleline(&mut state.task_proj)
                .desired_width(80.0).font(egui::TextStyle::Monospace));
            egui::ComboBox::from_id_salt("task_prio").width(56.0)
                .selected_text(&state.task_prio)
                .show_ui(ui, |ui| {
                    for p in ["P0","P1","P2"] {
                        ui.selectable_value(&mut state.task_prio, p.to_string(), p);
                    }
                });
        });
        ui.add(egui::TextEdit::singleline(&mut state.task_desc)
            .desired_width(f32::INFINITY)
            .hint_text("Task description…")
            .font(egui::TextStyle::Body));
        if ui.add_enabled(
            !state.task_desc.is_empty(),
            egui::Button::new(RichText::new("+ Create Task").size(theme::FONT_SMALL))
        ).clicked() {
            let args = format!(
                r#"{{"project":"{}","description":{},"priority":"{}"}}"#,
                state.task_proj,
                serde_json::to_string(&state.task_desc).unwrap_or_default(),
                state.task_prio
            );
            let r = svc.mcp_call("sirin/create_task", &args)
                .unwrap_or_else(|e| format!("❌ {e}"));
            state.tasks_raw = r;
            state.task_desc.clear();
        }

        ui.add_space(theme::SP_LG);

        // ── Save Points ────────────────────────────────────────────────
        theme::section_header(ui, "Save Points");
        ui.horizontal(|ui| {
            if ui.button(RichText::new("↻ Load").size(theme::FONT_SMALL)).clicked() {
                state.points_raw = svc.mcp_call("sirin/list_points", "{}")
                    .unwrap_or_else(|e| format!("❌ {e}"));
            }
        });
        if !state.points_raw.is_empty() {
            egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM)
                .show(ui, |ui| {
                    ui.colored_label(theme::TEXT_DIM,
                        RichText::new(&state.points_raw).size(theme::FONT_CAPTION).monospace());
                });
        }
        ui.add_space(theme::SP_XS);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut state.point_label)
                .desired_width(120.0).font(egui::TextStyle::Monospace)
                .hint_text("label…"));
            ui.add(egui::TextEdit::singleline(&mut state.point_summary)
                .desired_width(200.0)
                .hint_text("summary (optional)…"));
            if ui.add_enabled(
                !state.point_label.is_empty(),
                egui::Button::new(RichText::new("💾 Save Point").size(theme::FONT_SMALL))
            ).clicked() {
                let args = format!(
                    r#"{{"label":"{}","summary":{}}}"#,
                    state.point_label,
                    serde_json::to_string(&state.point_summary).unwrap_or_default()
                );
                let r = svc.mcp_call("sirin/save_point", &args)
                    .unwrap_or_else(|e| format!("❌ {e}"));
                state.points_raw = r;
                state.point_label.clear();
                state.point_summary.clear();
            }
        });
    });
}

// ── Cost & KB tab ─────────────────────────────────────────────────────────────

fn show_cost_kb(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut OpsPanelState) {
    ScrollArea::vertical().id_salt("ops_cost").show(ui, |ui| {

        // ── Session Cost ───────────────────────────────────────────────
        theme::section_header(ui, "Session Cost");
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("Project").size(theme::FONT_SMALL));
            ui.add(egui::TextEdit::singleline(&mut state.cost_project)
                .desired_width(120.0).font(egui::TextStyle::Monospace));
            if ui.button(RichText::new("↻ Top 10").size(theme::FONT_SMALL)).clicked() {
                let args = format!(r#"{{"top":10,"project_key":"{}"}}"#, state.cost_project);
                state.cost_raw = svc.mcp_call("sirin/list_expensive_sessions", &args)
                    .unwrap_or_else(|e| format!("❌ {e}"));
            }
            if ui.button(RichText::new("Latest").size(theme::FONT_SMALL)).clicked() {
                let args = format!(r#"{{"project_key":"{}"}}"#, state.cost_project);
                state.cost_raw = svc.mcp_call("sirin/session_cost", &args)
                    .unwrap_or_else(|e| format!("❌ {e}"));
            }
        });
        if !state.cost_raw.is_empty() {
            egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM)
                .show(ui, |ui| {
                    ScrollArea::vertical().id_salt("cost_raw").max_height(180.0).show(ui, |ui| {
                        ui.colored_label(theme::ACCENT,
                            RichText::new(&state.cost_raw).size(theme::FONT_SMALL).monospace());
                    });
                });
        }

        ui.add_space(theme::SP_LG);

        // ── KB Stats ───────────────────────────────────────────────────
        theme::section_header(ui, "KB Stats");
        ui.horizontal(|ui| {
            for proj in ["sirin", "agora-backend", "flutter"] {
                if ui.button(RichText::new(proj).size(theme::FONT_SMALL)).clicked() {
                    let args = format!(r#"{{"project":"{}"}}"#, proj);
                    state.kb_stats_raw = svc.mcp_call("sirin/kb_stats", &args)
                        .unwrap_or_else(|e| format!("❌ {e}"));
                }
            }
        });
        if !state.kb_stats_raw.is_empty() {
            egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM)
                .show(ui, |ui| {
                    ScrollArea::vertical().id_salt("kb_stats").max_height(180.0).show(ui, |ui| {
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new(&state.kb_stats_raw).size(theme::FONT_SMALL).monospace());
                    });
                });
        }
    });
}
