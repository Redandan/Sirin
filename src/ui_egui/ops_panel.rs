//! OPS Panel — three sub-tabs using Sirin's local MCP (:7700).
//!
//!   [AI Router]      route_query / benchmark_llms / intent registry
//!   [Session & Tasks] tasks + save points
//!   [Cost & KB]      session cost leaderboard / kb_stats

use std::sync::{Arc, Mutex};
use eframe::egui::{self, RichText, ScrollArea};
use crate::ui_service::AppService;
use super::theme;

// ── Tabs ──────────────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum OpsTab { #[default] AiRouter, SessionTasks, CostKb }

// ── Async result cell ─────────────────────────────────────────────────────────

/// Shared cell for background MCP results.  Inner = None while running,
/// Some(Ok/Err) when done.  Use spawn_sirin_call() to populate.
type ResultCell = Arc<Mutex<Option<Result<String, String>>>>;

fn new_cell() -> ResultCell { Arc::new(Mutex::new(None)) }

/// Spawn a Sirin local MCP call in a background thread.
/// When done, writes into `cell` and requests repaint on `ctx`.
fn spawn_sirin_call(
    svc:  &Arc<dyn AppService>,
    ctx:  egui::Context,
    cell: ResultCell,
    tool: &str,
    args: String,
) {
    let svc2  = svc.clone();
    let tool2 = tool.to_string();
    *cell.lock().unwrap() = None; // mark as running
    std::thread::spawn(move || {
        let res = svc2.sirin_mcp_call(&tool2, &args);
        *cell.lock().unwrap() = Some(res);
        ctx.request_repaint();
    });
}

/// Poll a ResultCell.  Returns (is_running, result_text).
fn poll_cell(cell: &ResultCell) -> (bool, Option<String>) {
    match cell.lock().unwrap().as_ref() {
        None          => (true,  None),                    // still running
        Some(Ok(s))   => (false, Some(s.clone())),
        Some(Err(e))  => (false, Some(format!("❌ {e}"))),
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

pub struct OpsPanelState {
    pub tab: OpsTab,

    // AI Router
    ai_intent:   String,
    ai_prompt:   String,
    ai_cell:     ResultCell,
    bench_cell:  ResultCell,
    intents_cell: ResultCell,
    new_intent_name:    String,
    new_intent_backend: String,
    reg_cell:    ResultCell,

    // Session & Tasks
    tasks_cell:  ResultCell,
    task_desc:   String,
    task_proj:   String,
    task_prio:   String,
    create_task_cell: ResultCell,
    points_cell: ResultCell,
    point_label: String,
    point_summary: String,
    save_point_cell: ResultCell,

    // Cost & KB
    cost_cell:   ResultCell,
    kb_cell:     ResultCell,
    cost_project: String,
}

impl Default for OpsPanelState {
    fn default() -> Self {
        Self {
            tab: OpsTab::default(),
            ai_intent:   "code-review".to_string(),
            ai_prompt:   String::new(),
            ai_cell:     new_cell(),
            bench_cell:  new_cell(),
            intents_cell: new_cell(),
            new_intent_name:    String::new(),
            new_intent_backend: "gemini".to_string(),
            reg_cell:    new_cell(),
            tasks_cell:  new_cell(),
            task_desc:   String::new(),
            task_proj:   "sirin".to_string(),
            task_prio:   "P1".to_string(),
            create_task_cell: new_cell(),
            points_cell: new_cell(),
            point_label: String::new(),
            point_summary: String::new(),
            save_point_cell: new_cell(),
            cost_cell:   new_cell(),
            kb_cell:     new_cell(),
            cost_project: "sirin".to_string(),
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut OpsPanelState) {
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
    let sel  = *active == tab;
    let text = RichText::new(label).size(theme::FONT_SMALL).strong();
    let text = if sel { text.color(theme::ACCENT) } else { text.color(theme::TEXT_DIM) };
    if ui.add(egui::Button::new(text).frame(false)).clicked() { *active = tab; }
    if sel {
        let r = ui.min_rect();
        ui.painter().hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(2.0, theme::ACCENT));
    }
}

// ── Result display helper ─────────────────────────────────────────────────────

fn result_box(ui: &mut egui::Ui, cell: &ResultCell, id: &str) {
    let (running, text) = poll_cell(cell);
    if running {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(14.0).color(theme::ACCENT));
            ui.colored_label(theme::TEXT_DIM, RichText::new("running…").size(theme::FONT_CAPTION));
        });
    } else if let Some(txt) = text {
        egui::Frame::new().fill(theme::BG).corner_radius(4.0).inner_margin(theme::SP_SM)
            .show(ui, |ui| {
                ScrollArea::vertical().id_salt(id).max_height(160.0).show(ui, |ui| {
                    ui.colored_label(theme::ACCENT,
                        RichText::new(&txt).size(theme::FONT_SMALL).monospace());
                });
            });
    }
}

// ── AI Router tab ─────────────────────────────────────────────────────────────

fn show_ai_router(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut OpsPanelState) {
    let ctx = ui.ctx().clone();
    ScrollArea::vertical().id_salt("ops_ai").show(ui, |ui| {

        // ── Route Query ──────────────────────────────────────────────
        theme::section_header(ui, "Route Query");
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("Intent").size(theme::FONT_SMALL));
            ui.add(egui::TextEdit::singleline(&mut state.ai_intent)
                .desired_width(140.0).font(egui::TextStyle::Monospace)
                .hint_text("e.g. code-review"));
        });
        ui.add_space(theme::SP_XS);
        ui.add(egui::TextEdit::multiline(&mut state.ai_prompt)
            .desired_width(f32::INFINITY).desired_rows(3)
            .hint_text("Enter prompt…").font(egui::TextStyle::Body));
        ui.add_space(theme::SP_XS);

        let (ai_running, _) = poll_cell(&state.ai_cell);
        let (bench_running, _) = poll_cell(&state.bench_cell);
        let prompt_ok = !state.ai_prompt.is_empty();

        ui.horizontal(|ui| {
            let route_btn = egui::Button::new(
                RichText::new("▶ Route Query").size(theme::FONT_SMALL).color(theme::BG)
            ).fill(theme::ACCENT).corner_radius(4.0);
            if ui.add_enabled(prompt_ok && !ai_running, route_btn).clicked() {
                let args = format!(
                    r#"{{"intent":"{}","prompt":{}}}"#,
                    state.ai_intent,
                    serde_json::to_string(&state.ai_prompt).unwrap_or_default()
                );
                spawn_sirin_call(svc, ctx.clone(), state.ai_cell.clone(), "route_query", args);
            }

            ui.add_space(theme::SP_SM);

            let bench_btn = egui::Button::new(
                RichText::new("⚡ Benchmark").size(theme::FONT_SMALL).color(theme::BG)
            ).fill(theme::INFO).corner_radius(4.0);
            if ui.add_enabled(prompt_ok && !bench_running, bench_btn).clicked() {
                let args = format!(
                    r#"{{"prompt":{},"backends":"gemini,deepseek"}}"#,
                    serde_json::to_string(&state.ai_prompt).unwrap_or_default()
                );
                spawn_sirin_call(svc, ctx.clone(), state.bench_cell.clone(), "benchmark_llms", args);
            }
        });

        result_box(ui, &state.ai_cell,   "ai_result");
        result_box(ui, &state.bench_cell, "bench_result");
        ui.add_space(theme::SP_LG);

        // ── Intent Registry ──────────────────────────────────────────
        theme::section_header(ui, "Intent Registry");
        let (intents_running, _) = poll_cell(&state.intents_cell);
        ui.horizontal(|ui| {
            if ui.add_enabled(!intents_running,
                egui::Button::new(RichText::new("↻ Load").size(theme::FONT_SMALL))
            ).clicked() {
                spawn_sirin_call(svc, ctx.clone(), state.intents_cell.clone(),
                    "list_intents", "{}".to_string());
            }
        });
        result_box(ui, &state.intents_cell, "intents");
        ui.add_space(theme::SP_XS);

        // Register form.
        let (reg_running, _) = poll_cell(&state.reg_cell);
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("Name").size(theme::FONT_SMALL));
            ui.add(egui::TextEdit::singleline(&mut state.new_intent_name)
                .desired_width(120.0).font(egui::TextStyle::Monospace));
            egui::ComboBox::from_id_salt("intent_backend").width(100.0)
                .selected_text(&state.new_intent_backend)
                .show_ui(ui, |ui| {
                    for b in ["gemini","deepseek","claude","ollama"] {
                        ui.selectable_value(&mut state.new_intent_backend, b.to_string(), b);
                    }
                });
            if ui.add_enabled(
                !state.new_intent_name.is_empty() && !reg_running,
                egui::Button::new(RichText::new("+ Register").size(theme::FONT_SMALL))
            ).clicked() {
                let args = format!(
                    r#"{{"name":"{}","backend":"{}"}}"#,
                    state.new_intent_name, state.new_intent_backend
                );
                spawn_sirin_call(svc, ctx.clone(), state.reg_cell.clone(),
                    "register_intent", args);
                state.new_intent_name.clear();
            }
        });
        result_box(ui, &state.reg_cell, "reg_result");
    });
}

// ── Session & Tasks tab ───────────────────────────────────────────────────────

fn show_session_tasks(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut OpsPanelState) {
    let ctx = ui.ctx().clone();
    ScrollArea::vertical().id_salt("ops_session").show(ui, |ui| {

        // ── Tasks ────────────────────────────────────────────────────
        theme::section_header(ui, "Tasks");
        let (tasks_running, _) = poll_cell(&state.tasks_cell);
        ui.horizontal(|ui| {
            if ui.add_enabled(!tasks_running,
                egui::Button::new(RichText::new("↻ Open").size(theme::FONT_SMALL))
            ).clicked() {
                spawn_sirin_call(svc, ctx.clone(), state.tasks_cell.clone(),
                    "list_tasks", r#"{"status":"open"}"#.to_string());
            }
            if ui.add_enabled(!tasks_running,
                egui::Button::new(RichText::new("All").size(theme::FONT_SMALL))
            ).clicked() {
                spawn_sirin_call(svc, ctx.clone(), state.tasks_cell.clone(),
                    "list_tasks", r#"{"status":"all"}"#.to_string());
            }
        });
        result_box(ui, &state.tasks_cell, "tasks_list");
        ui.add_space(theme::SP_XS);

        // Create form.
        let (create_running, _) = poll_cell(&state.create_task_cell);
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("Proj").size(theme::FONT_SMALL));
            ui.add(egui::TextEdit::singleline(&mut state.task_proj)
                .desired_width(72.0).font(egui::TextStyle::Monospace));
            egui::ComboBox::from_id_salt("task_prio").width(52.0)
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
            !state.task_desc.is_empty() && !create_running,
            egui::Button::new(RichText::new("+ Create Task").size(theme::FONT_SMALL))
        ).clicked() {
            let args = format!(
                r#"{{"project":"{}","description":{},"priority":"{}"}}"#,
                state.task_proj,
                serde_json::to_string(&state.task_desc).unwrap_or_default(),
                state.task_prio
            );
            spawn_sirin_call(svc, ctx.clone(), state.create_task_cell.clone(),
                "create_task", args);
            state.task_desc.clear();
        }
        result_box(ui, &state.create_task_cell, "create_task_result");

        ui.add_space(theme::SP_LG);

        // ── Save Points ──────────────────────────────────────────────
        theme::section_header(ui, "Save Points");
        let (points_running, _) = poll_cell(&state.points_cell);
        if ui.add_enabled(!points_running,
            egui::Button::new(RichText::new("↻ Load").size(theme::FONT_SMALL))
        ).clicked() {
            spawn_sirin_call(svc, ctx.clone(), state.points_cell.clone(),
                "list_points", "{}".to_string());
        }
        result_box(ui, &state.points_cell, "points_list");
        ui.add_space(theme::SP_XS);

        let (save_running, _) = poll_cell(&state.save_point_cell);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut state.point_label)
                .desired_width(120.0).font(egui::TextStyle::Monospace)
                .hint_text("label…"));
            ui.add(egui::TextEdit::singleline(&mut state.point_summary)
                .desired_width(200.0).hint_text("summary (optional)…"));
            if ui.add_enabled(
                !state.point_label.is_empty() && !save_running,
                egui::Button::new(RichText::new("💾 Save").size(theme::FONT_SMALL))
            ).clicked() {
                let args = format!(
                    r#"{{"label":"{}","summary":{}}}"#,
                    state.point_label,
                    serde_json::to_string(&state.point_summary).unwrap_or_default()
                );
                spawn_sirin_call(svc, ctx.clone(), state.save_point_cell.clone(),
                    "save_point", args);
                state.point_label.clear();
                state.point_summary.clear();
            }
        });
        result_box(ui, &state.save_point_cell, "save_point_result");
    });
}

// ── Cost & KB tab ─────────────────────────────────────────────────────────────

fn show_cost_kb(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut OpsPanelState) {
    let ctx = ui.ctx().clone();
    ScrollArea::vertical().id_salt("ops_cost").show(ui, |ui| {

        // ── Session Cost ─────────────────────────────────────────────
        theme::section_header(ui, "Session Cost");
        let (cost_running, _) = poll_cell(&state.cost_cell);
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT_DIM, RichText::new("Project").size(theme::FONT_SMALL));
            ui.add(egui::TextEdit::singleline(&mut state.cost_project)
                .desired_width(120.0).font(egui::TextStyle::Monospace));
            if ui.add_enabled(!cost_running,
                egui::Button::new(RichText::new("↻ Top 10").size(theme::FONT_SMALL))
            ).clicked() {
                let args = format!(r#"{{"top":10,"project_key":"{}"}}"#, state.cost_project);
                spawn_sirin_call(svc, ctx.clone(), state.cost_cell.clone(),
                    "list_expensive_sessions", args);
            }
            if ui.add_enabled(!cost_running,
                egui::Button::new(RichText::new("Latest").size(theme::FONT_SMALL))
            ).clicked() {
                let args = format!(r#"{{"project_key":"{}"}}"#, state.cost_project);
                spawn_sirin_call(svc, ctx.clone(), state.cost_cell.clone(),
                    "session_cost", args);
            }
        });
        result_box(ui, &state.cost_cell, "cost_result");

        ui.add_space(theme::SP_LG);

        // ── KB Stats ─────────────────────────────────────────────────
        theme::section_header(ui, "KB Stats");
        let (kb_running, _) = poll_cell(&state.kb_cell);
        ui.horizontal(|ui| {
            for proj in ["sirin","agora-backend","flutter"] {
                if ui.add_enabled(!kb_running,
                    egui::Button::new(RichText::new(proj).size(theme::FONT_SMALL))
                ).clicked() {
                    let args = format!(r#"{{"project":"{}"}}"#, proj);
                    spawn_sirin_call(svc, ctx.clone(), state.kb_cell.clone(), "kb_stats", args);
                }
            }
        });
        result_box(ui, &state.kb_cell, "kb_stats_result");
    });
}
