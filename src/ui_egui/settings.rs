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
    // Config diagnostics
    config_issues: Vec<ConfigIssueView>,
    config_issues_loaded: bool,
    // AI advice
    config_ai_loading: bool,
    config_ai_advice: Option<AiAdviceView>,
    config_ai_selected: Vec<bool>,
    config_ai_error: Option<String>,
    config_ai_confirm: bool,
    config_ai_rx: Option<std::sync::mpsc::Receiver<Result<AiAdviceView, String>>>,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, _agents: &[AgentSummary], state: &mut SettingsState) {
    ScrollArea::vertical().id_salt("system_settings").show(ui, |ui| {
        ui.set_max_width(560.0);
        let s = svc.system_status();

        // Lazy-load config issues on first render
        if !state.config_issues_loaded {
            state.config_issues = svc.config_check();
            state.config_issues_loaded = true;
        }

        // ── Config diagnostics ────────────────────────────────────────────
        show_config_diagnostics(ui, svc, state);

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

        // MCP Playground moved to its own sidebar view (AUTOMATION → MCP Playground).
        theme::section(ui, "MCP Playground", |ui| {
            ui.colored_label(theme::TEXT_DIM,
                RichText::new("MCP 工具已移至側欄「AUTOMATION → MCP Playground」")
                    .size(theme::FONT_SMALL));
            ui.add_space(theme::SP_XS);
            ui.colored_label(theme::INFO,
                RichText::new(format!("{} tools available", svc.mcp_tools().len()))
                    .size(theme::FONT_SMALL).monospace());
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

        // ── Dev tools (collapsed by default) ────────────────────────────
        ui.add_space(theme::SP_SM);
        egui::CollapsingHeader::new(
            RichText::new("▸ 開發者工具").size(theme::FONT_SMALL).color(theme::TEXT_DIM)
        ).default_open(false).show(ui, |ui| {
            // Research trigger
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

            // Skill test
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

            // Config export/import
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
        });
    });
}

// ── Config diagnostics section ──────────────────────────────────────────────

fn show_config_diagnostics(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut SettingsState) {
    theme::section(ui, "系統診斷", |ui| {
        let errors = state.config_issues.iter().filter(|i| i.severity == ConfigSeverity::Error).count();
        let warnings = state.config_issues.iter().filter(|i| i.severity == ConfigSeverity::Warning).count();
        let oks = state.config_issues.iter().filter(|i| i.severity == ConfigSeverity::Ok).count();

        // Summary + refresh button
        ui.horizontal(|ui| {
            ui.colored_label(theme::ACCENT, RichText::new(format!("{oks} OK")).size(theme::FONT_SMALL).strong());
            ui.colored_label(theme::TEXT_DIM, "·");
            if warnings > 0 {
                ui.colored_label(theme::YELLOW, RichText::new(format!("{warnings} warnings")).size(theme::FONT_SMALL).strong());
            } else {
                ui.colored_label(theme::TEXT_DIM, RichText::new("0 warnings").size(theme::FONT_SMALL));
            }
            ui.colored_label(theme::TEXT_DIM, "·");
            if errors > 0 {
                ui.colored_label(theme::DANGER, RichText::new(format!("{errors} errors")).size(theme::FONT_SMALL).strong());
            } else {
                ui.colored_label(theme::TEXT_DIM, RichText::new("0 errors").size(theme::FONT_SMALL));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("🔄 重新檢查").size(theme::FONT_SMALL))
                    .fill(theme::CARD).corner_radius(4.0)).clicked()
                {
                    state.config_issues = svc.config_check();
                }
            });
        });

        ui.add_space(theme::SP_SM);

        // Issues list — show warnings/errors first, then OK
        let issues_to_show: Vec<&ConfigIssueView> = state.config_issues.iter()
            .filter(|i| i.severity != ConfigSeverity::Ok)
            .collect();

        if issues_to_show.is_empty() {
            ui.colored_label(theme::ACCENT, RichText::new("✓ 所有檢查通過").size(theme::FONT_SMALL));
        } else {
            for issue in &issues_to_show {
                render_issue(ui, issue);
            }
        }

        // Collapsible OK items
        if oks > 0 {
            ui.add_space(theme::SP_SM);
            ui.collapsing(
                RichText::new(format!("顯示 {oks} 項通過檢查")).size(theme::FONT_CAPTION).color(theme::TEXT_DIM),
                |ui| {
                    for issue in state.config_issues.iter().filter(|i| i.severity == ConfigSeverity::Ok) {
                        render_issue(ui, issue);
                    }
                },
            );
        }

        // ── AI analysis panel ──────────────────────────────────────────
        ui.add_space(theme::SP_MD);
        ui.separator();
        ui.add_space(theme::SP_SM);
        show_ai_panel(ui, svc, state);
    });
}

fn show_ai_panel(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut SettingsState) {
    // Poll async result if loading
    if let Some(rx) = &state.config_ai_rx {
        if let Ok(result) = rx.try_recv() {
            state.config_ai_loading = false;
            state.config_ai_rx = None;
            match result {
                Ok(advice) => {
                    state.config_ai_selected = vec![true; advice.proposed_fixes.len()];
                    state.config_ai_advice = Some(advice);
                    state.config_ai_error = None;
                }
                Err(e) => {
                    state.config_ai_error = Some(e);
                    state.config_ai_advice = None;
                }
            }
        }
    }

    // Header row
    ui.horizontal(|ui| {
        ui.colored_label(theme::INFO, RichText::new("AI 分析與自動修復").size(theme::FONT_BODY).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let btn_label = if state.config_ai_loading { "分析中..." } else { "🧠 AI 分析" };
            if ui.add_enabled(!state.config_ai_loading,
                egui::Button::new(RichText::new(btn_label).color(theme::BG))
                    .fill(theme::INFO).corner_radius(4.0)).clicked()
            {
                state.config_ai_loading = true;
                state.config_ai_error = None;
                state.config_ai_advice = None;
                let (tx, rx) = std::sync::mpsc::channel();
                state.config_ai_rx = Some(rx);
                let svc = svc.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(svc.config_ai_analyze());
                });
            }
        });
    });

    if state.config_ai_loading {
        ui.add_space(theme::SP_SM);
        ui.colored_label(theme::YELLOW, RichText::new("⏳ 呼叫 Gemini 2.5 Pro 分析中，請稍候...").size(theme::FONT_SMALL));
        return;
    }

    if let Some(err) = &state.config_ai_error {
        ui.add_space(theme::SP_SM);
        egui::Frame::new().fill(theme::DANGER.linear_multiply(0.12)).corner_radius(4.0)
            .inner_margin(theme::SP_SM).show(ui, |ui| {
                ui.colored_label(theme::DANGER, RichText::new(format!("✗ {err}")).size(theme::FONT_SMALL));
            });
        return;
    }

    let Some(advice) = state.config_ai_advice.clone() else {
        ui.add_space(theme::SP_SM);
        ui.colored_label(theme::TEXT_DIM, RichText::new("按「AI 分析」讓 Gemini 分析你的配置並提出建議（修改前會先確認）").size(theme::FONT_CAPTION));
        return;
    };

    // Analysis section
    ui.add_space(theme::SP_SM);
    egui::Frame::new().fill(theme::CARD).corner_radius(4.0)
        .inner_margin(theme::SP_MD).show(ui, |ui| {
            ui.label(RichText::new("AI 分析").size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
            ui.add_space(theme::SP_XS);
            ui.label(RichText::new(&advice.analysis).size(theme::FONT_SMALL).color(theme::TEXT));
        });

    if advice.proposed_fixes.is_empty() {
        ui.add_space(theme::SP_SM);
        ui.colored_label(theme::ACCENT, RichText::new("✓ 沒有建議的修改 — 配置看起來很好").size(theme::FONT_SMALL));
        return;
    }

    // Proposed fixes with checkboxes
    ui.add_space(theme::SP_SM);
    ui.label(RichText::new(format!("建議修改 ({})", advice.proposed_fixes.len())).size(theme::FONT_SMALL).color(theme::TEXT_DIM));
    ui.add_space(theme::SP_XS);

    // Ensure selected vec matches advice length
    if state.config_ai_selected.len() != advice.proposed_fixes.len() {
        state.config_ai_selected = vec![true; advice.proposed_fixes.len()];
    }

    for (idx, fix) in advice.proposed_fixes.iter().enumerate() {
        egui::Frame::new().fill(theme::CARD).corner_radius(4.0)
            .inner_margin(theme::SP_SM).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut state.config_ai_selected[idx], "");
                    theme::badge(ui, &fix.file, theme::INFO);
                    ui.colored_label(theme::TEXT, RichText::new(&fix.field_path)
                        .size(theme::FONT_SMALL).family(egui::FontFamily::Monospace));
                });
                ui.horizontal(|ui| {
                    ui.add_space(24.0);
                    ui.colored_label(theme::TEXT_DIM, RichText::new(
                        if fix.current_value.is_empty() { "(empty)".to_string() }
                        else { format!("\"{}\"", fix.current_value) }
                    ).size(theme::FONT_CAPTION).family(egui::FontFamily::Monospace));
                    ui.colored_label(theme::TEXT_DIM, "→");
                    ui.colored_label(theme::ACCENT, RichText::new(format!("\"{}\"", fix.new_value))
                        .size(theme::FONT_CAPTION).family(egui::FontFamily::Monospace));
                });
                if !fix.reason.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        ui.colored_label(theme::TEXT_DIM, RichText::new(&fix.reason).size(theme::FONT_CAPTION));
                    });
                }
            });
        ui.add_space(4.0);
    }

    // Action buttons
    let selected_count = state.config_ai_selected.iter().filter(|&&b| b).count();
    ui.add_space(theme::SP_SM);
    ui.horizontal(|ui| {
        if ui.button(RichText::new("全選").size(theme::FONT_SMALL)).clicked() {
            state.config_ai_selected = vec![true; advice.proposed_fixes.len()];
        }
        if ui.button(RichText::new("全不選").size(theme::FONT_SMALL)).clicked() {
            state.config_ai_selected = vec![false; advice.proposed_fixes.len()];
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add_enabled(selected_count > 0,
                egui::Button::new(RichText::new(format!("✓ 套用選中的 ({selected_count})")).color(theme::BG))
                    .fill(theme::ACCENT).corner_radius(4.0)).clicked()
            {
                state.config_ai_confirm = true;
            }
        });
    });

    // Final confirmation dialog
    if state.config_ai_confirm {
        show_confirm_dialog(ui.ctx(), svc, state, &advice);
    }
}

fn show_confirm_dialog(
    ctx: &egui::Context,
    svc: &Arc<dyn AppService>,
    state: &mut SettingsState,
    advice: &AiAdviceView,
) {
    egui::Window::new("確認套用配置修改")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let selected: Vec<&ConfigFixView> = advice.proposed_fixes.iter()
                .enumerate()
                .filter(|(i, _)| state.config_ai_selected.get(*i).copied().unwrap_or(false))
                .map(|(_, f)| f)
                .collect();

            ui.set_max_width(500.0);
            ui.colored_label(theme::YELLOW, RichText::new("⚠ 即將套用以下修改").strong());
            ui.add_space(theme::SP_SM);
            ui.colored_label(theme::TEXT_DIM, RichText::new("原檔會先備份為 .bak.TIMESTAMP。重啟 Sirin 後生效。").size(theme::FONT_SMALL));
            ui.add_space(theme::SP_MD);

            for fix in &selected {
                ui.horizontal(|ui| {
                    theme::badge(ui, &fix.file, theme::INFO);
                    ui.label(RichText::new(&fix.field_path).family(egui::FontFamily::Monospace).size(theme::FONT_SMALL));
                });
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.colored_label(theme::DANGER, RichText::new(format!("- \"{}\"", fix.current_value))
                        .family(egui::FontFamily::Monospace).size(theme::FONT_CAPTION));
                });
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.colored_label(theme::ACCENT, RichText::new(format!("+ \"{}\"", fix.new_value))
                        .family(egui::FontFamily::Monospace).size(theme::FONT_CAPTION));
                });
                ui.add_space(4.0);
            }

            ui.add_space(theme::SP_MD);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("取消").color(theme::TEXT))
                    .fill(theme::CARD).corner_radius(4.0)).clicked()
                {
                    state.config_ai_confirm = false;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new("✓ 確定套用").color(theme::BG))
                        .fill(theme::ACCENT).corner_radius(4.0)).clicked()
                    {
                        let to_apply: Vec<ConfigFixView> = selected.iter().map(|f| (*f).clone()).collect();
                        match svc.config_apply_fixes(to_apply) {
                            Ok(applied) => {
                                state.config_ai_confirm = false;
                                state.config_ai_advice = None;
                                // Refresh diagnostics
                                state.config_issues = svc.config_check();
                                tracing::info!("Applied {} config fixes", applied.len());
                            }
                            Err(e) => {
                                state.config_ai_error = Some(format!("套用失敗: {e}"));
                                state.config_ai_confirm = false;
                            }
                        }
                    }
                });
            });
        });
}

fn render_issue(ui: &mut egui::Ui, issue: &ConfigIssueView) {
    let (color, icon) = match issue.severity {
        ConfigSeverity::Ok => (theme::ACCENT, "✓"),
        ConfigSeverity::Info => (theme::INFO, "ℹ"),
        ConfigSeverity::Warning => (theme::YELLOW, "⚠"),
        ConfigSeverity::Error => (theme::DANGER, "✗"),
    };
    egui::Frame::new().fill(theme::CARD).corner_radius(4.0)
        .inner_margin(theme::SP_SM).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(color, RichText::new(icon).size(theme::FONT_BODY).strong());
                theme::badge(ui, &issue.category, color);
                ui.colored_label(theme::TEXT, RichText::new(&issue.message).size(theme::FONT_SMALL));
            });
            if let Some(s) = &issue.suggestion {
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.colored_label(theme::TEXT_DIM, RichText::new(format!("→ {s}")).size(theme::FONT_CAPTION));
                });
            }
        });
    ui.add_space(4.0);
}
