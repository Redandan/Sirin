//! 開發小隊監控面板
//!
//! 顯示 PM / Engineer / Tester 即時狀態，以及持久化任務佇列。
//! Worker 執行中時每 2 秒自動刷新。

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::theme;
use crate::ui_service::*;

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct TeamPanelState {
    new_task: String,
    last_refresh: Option<std::time::Instant>,
    dash: Option<TeamDashView>,
    tasks: Vec<TeamTaskView>,
    expanded_id: Option<String>,
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut TeamPanelState) {
    // Auto-refresh: every 2s while Worker is running, else on first show only
    let worker_running = state.dash.as_ref().map(|d| d.worker_running).unwrap_or(false);
    let refresh_interval = if worker_running {
        std::time::Duration::from_secs(2)
    } else {
        std::time::Duration::from_secs(10)
    };

    let should_refresh = state
        .last_refresh
        .map(|t| t.elapsed() > refresh_interval)
        .unwrap_or(true);

    if should_refresh {
        state.dash  = Some(svc.team_dashboard());
        state.tasks = svc.team_queue();
        state.last_refresh = Some(std::time::Instant::now());
    }

    // Keep UI live while worker is active
    if worker_running {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(2));
    }

    let dash = match state.dash.clone() {
        Some(d) => d,
        None => return,
    };

    ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            show_header(ui, svc, &dash, state);
            ui.add_space(theme::SP_MD);
            show_member_cards(ui, svc, &dash, state);
            ui.add_space(theme::SP_MD);
            theme::thin_separator(ui);
            ui.add_space(theme::SP_SM);
            show_queue_section(ui, svc, &dash, state);
        });
}

// ── Header ────────────────────────────────────────────────────────────────────

fn show_header(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    dash: &TeamDashView,
    state: &mut TeamPanelState,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("開發小隊")
                .size(theme::FONT_HEADING)
                .strong()
                .color(theme::TEXT),
        );
        ui.add_space(theme::SP_SM);

        // Worker status indicator
        let (dot_color, status_txt) = if dash.worker_running {
            (theme::ACCENT, "● Worker 執行中")
        } else {
            (theme::TEXT_DIM, "○ Worker 閒置")
        };
        ui.colored_label(dot_color, RichText::new(status_txt).size(theme::FONT_SMALL));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Clear completed
            if dash.done + dash.failed > 0 {
                if ui.add(
                    egui::Button::new(
                        RichText::new("清除完成").size(theme::FONT_SMALL).color(theme::TEXT_DIM),
                    )
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::new(0.5, theme::BORDER)),
                )
                .clicked()
                {
                    svc.team_clear_completed();
                    state.last_refresh = None;
                }
                ui.add_space(theme::SP_SM);
            }

            // Start worker
            if !dash.worker_running {
                if ui.add(
                    egui::Button::new(
                        RichText::new("▶ 啟動 Worker")
                            .size(theme::FONT_SMALL)
                            .color(theme::ACCENT),
                    )
                    .fill(theme::ACCENT.linear_multiply(0.1))
                    .stroke(egui::Stroke::new(1.0, theme::ACCENT.linear_multiply(0.4)))
                    .corner_radius(4.0),
                )
                .clicked()
                {
                    svc.team_start_worker();
                    state.last_refresh = None;
                }
            }
        });
    });
}

// ── Member cards ──────────────────────────────────────────────────────────────

fn show_member_cards(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    dash: &TeamDashView,
    state: &mut TeamPanelState,
) {
    let total_w  = ui.available_width();
    let gap      = theme::SP_SM;
    let card_w   = (total_w - gap * 2.0) / 3.0;

    ui.horizontal(|ui| {
        for member in [&dash.pm, &dash.engineer, &dash.tester] {
            member_card(ui, svc, member, card_w, state);
            ui.add_space(gap);
        }
    });
}

fn member_card(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    member: &TeamMemberView,
    width: f32,
    state: &mut TeamPanelState,
) {
    let active       = member.session_id.is_some();
    let border_color = if active {
        theme::ACCENT.linear_multiply(0.35)
    } else {
        theme::BORDER
    };

    egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(1.0, border_color))
        .inner_margin(theme::SP_MD)
        .show(ui, |ui| {
            ui.set_width(width);

            // Role title + dot
            ui.horizontal(|ui| {
                let (dot, dot_color) = if active {
                    ("●", theme::ACCENT)
                } else {
                    ("○", theme::TEXT_DIM)
                };
                ui.colored_label(dot_color, dot);
                ui.label(
                    RichText::new(role_display(&member.role))
                        .strong()
                        .size(theme::FONT_SMALL)
                        .color(theme::TEXT),
                );
            });

            ui.add_space(theme::SP_XS);

            // Session ID
            let sid_text = match &member.session_id {
                Some(id) => {
                    let n = id.chars().count().min(12);
                    let short: String = id.chars().take(n).collect();
                    format!("#{short}…")
                }
                None => "尚未開始".to_string(),
            };
            ui.label(
                RichText::new(&sid_text)
                    .size(theme::FONT_CAPTION)
                    .color(theme::TEXT_DIM),
            );

            // Turns
            ui.label(
                RichText::new(format!("對話輪次: {}", member.turns))
                    .size(theme::FONT_CAPTION)
                    .color(theme::TEXT_DIM),
            );

            ui.add_space(theme::SP_XS);

            // Reset button (bottom right aligned)
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if ui.add(
                    egui::Button::new(
                        RichText::new("重置").size(theme::FONT_CAPTION).color(theme::DANGER),
                    )
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::new(0.5, theme::DANGER.linear_multiply(0.4)))
                    .corner_radius(3.0),
                )
                .on_hover_text(format!("清除 {} 的 session，開始全新對話", member.role))
                .clicked()
                {
                    svc.team_reset_member(&member.role);
                    state.last_refresh = None;
                }
            });
        });
}

fn role_display(role: &str) -> &str {
    match role {
        "pm"       => "PM",
        "engineer" => "Engineer",
        "tester"   => "Tester",
        other      => other,
    }
}

// ── Queue section ─────────────────────────────────────────────────────────────

fn show_queue_section(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    dash: &TeamDashView,
    state: &mut TeamPanelState,
) {
    // Queue title + counts
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("任務佇列")
                .size(theme::FONT_SMALL)
                .strong()
                .color(theme::TEXT),
        );
        ui.add_space(theme::SP_SM);

        if dash.running > 0 {
            status_badge(ui, &format!("{} 執行中", dash.running), theme::ACCENT);
        }
        if dash.queued > 0 {
            status_badge(ui, &format!("{} 排隊中", dash.queued), theme::INFO);
        }
        if dash.done > 0 {
            status_badge(ui, &format!("{} 完成", dash.done), theme::TEXT_DIM);
        }
        if dash.failed > 0 {
            status_badge(ui, &format!("{} 失敗", dash.failed), theme::DANGER);
        }
    });

    ui.add_space(theme::SP_SM);

    // New task input row
    ui.horizontal(|ui| {
        let btn_w   = 72.0;
        let input_w = ui.available_width() - btn_w - theme::SP_SM;
        ui.add_sized(
            [input_w, 28.0],
            egui::TextEdit::singleline(&mut state.new_task)
                .hint_text("描述新任務…")
                .text_color(theme::TEXT),
        );
        let can_add = !state.new_task.trim().is_empty();
        if ui.add_enabled(
            can_add,
            egui::Button::new(
                RichText::new("加入佇列").size(theme::FONT_SMALL).color(theme::ACCENT),
            )
            .fill(theme::ACCENT.linear_multiply(0.1))
            .stroke(egui::Stroke::new(1.0, theme::ACCENT.linear_multiply(0.3)))
            .corner_radius(4.0),
        )
        .clicked()
        {
            let desc = state.new_task.trim().to_string();
            svc.team_enqueue(&desc);
            state.new_task.clear();
            state.last_refresh = None;
        }
    });

    ui.add_space(theme::SP_SM);

    // Task list
    let tasks = state.tasks.clone();
    if tasks.is_empty() {
        ui.add_space(theme::SP_LG);
        ui.vertical_centered(|ui| {
            ui.colored_label(
                theme::TEXT_DIM,
                "佇列空白 — 輸入任務讓小隊開始工作",
            );
        });
        return;
    }

    for task in &tasks {
        task_card(ui, task, &mut state.expanded_id);
        ui.add_space(theme::SP_XS);
    }
}

// ── Task card ─────────────────────────────────────────────────────────────────

fn task_card(ui: &mut egui::Ui, task: &TeamTaskView, expanded_id: &mut Option<String>) {
    let (status_color, status_label, icon) = match task.status.as_str() {
        "running" => (theme::ACCENT,                             "執行中", "⟳"),
        "queued"  => (theme::INFO,                               "排隊中", "…"),
        "done"    => (egui::Color32::from_rgb(80, 180, 100),     "完成",   "✓"),
        "failed"  => (theme::DANGER,                             "失敗",   "✗"),
        _         => (theme::TEXT_DIM,                           "?",      "?"),
    };

    let is_expanded = expanded_id.as_deref() == Some(task.id.as_str());

    let frame_fill   = if task.status == "running" {
        theme::ACCENT.linear_multiply(0.04)
    } else {
        theme::CARD
    };
    let frame_border = if task.status == "running" {
        theme::ACCENT.linear_multiply(0.3)
    } else {
        theme::BORDER
    };

    egui::Frame::new()
        .fill(frame_fill)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(1.0, frame_border))
        .inner_margin(egui::vec2(theme::SP_MD, theme::SP_SM))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // ── Summary row (always visible) ─────────────────────────
            let row_resp = ui.horizontal(|ui| {
                // Status badge
                egui::Frame::new()
                    .fill(status_color.linear_multiply(0.15))
                    .corner_radius(3.0)
                    .inner_margin(egui::vec2(6.0, 2.0))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("{icon} {status_label}"))
                                .size(theme::FONT_CAPTION)
                                .color(status_color),
                        );
                    });

                ui.add_space(theme::SP_XS);

                // Description (truncated to ~70 chars)
                let desc: String = task.description.chars().take(70).collect();
                let desc_display = if task.description.chars().count() > 70 {
                    format!("{desc}…")
                } else {
                    desc
                };
                ui.label(
                    RichText::new(&desc_display)
                        .size(theme::FONT_SMALL)
                        .color(theme::TEXT),
                );

                // Expand chevron (right-aligned)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let chevron = if is_expanded { "▼" } else { "▶" };
                    ui.label(
                        RichText::new(chevron)
                            .size(theme::FONT_CAPTION)
                            .color(theme::TEXT_DIM),
                    );
                });
            });

            // Entire summary row is clickable
            if row_resp.response.interact(egui::Sense::click()).clicked() {
                if is_expanded {
                    *expanded_id = None;
                } else {
                    *expanded_id = Some(task.id.clone());
                }
            }

            // ── Expanded detail ──────────────────────────────────────
            if is_expanded {
                ui.add_space(theme::SP_XS);
                theme::thin_separator(ui);
                ui.add_space(theme::SP_XS);

                // Full description
                ui.label(
                    RichText::new(&task.description)
                        .size(theme::FONT_SMALL)
                        .color(theme::TEXT),
                );

                // Result preview
                if let Some(result) = &task.result {
                    ui.add_space(theme::SP_XS);
                    ui.label(RichText::new("結果：").size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
                    let preview: String = result.chars().take(300).collect();
                    let preview_text = if result.chars().count() > 300 {
                        format!("{preview}…")
                    } else {
                        preview
                    };
                    ui.label(
                        RichText::new(&preview_text)
                            .size(theme::FONT_CAPTION)
                            .color(theme::TEXT_DIM),
                    );
                }

                // Timestamps
                ui.add_space(theme::SP_XS);
                ui.horizontal(|ui| {
                    // created_at is RFC3339; show first 16 chars (YYYY-MM-DDTHH:MM)
                    let created = task.created_at.get(..16).unwrap_or(&task.created_at);
                    ui.label(
                        RichText::new(format!("建立 {}", created.replace('T', " ")))
                            .size(theme::FONT_CAPTION)
                            .color(theme::TEXT_DIM),
                    );
                    if let Some(fin) = &task.finished_at {
                        let fin_short = fin.get(..16).unwrap_or(fin);
                        ui.add_space(theme::SP_MD);
                        ui.label(
                            RichText::new(format!("完成 {}", fin_short.replace('T', " ")))
                                .size(theme::FONT_CAPTION)
                                .color(theme::TEXT_DIM),
                        );
                    }
                });
            }
        });
}

// ── Status badge (inline colored label) ──────────────────────────────────────

fn status_badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(color.linear_multiply(0.15))
        .corner_radius(3.0)
        .inner_margin(egui::vec2(6.0, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(theme::FONT_CAPTION).color(color));
        });
    ui.add_space(theme::SP_XS);
}
