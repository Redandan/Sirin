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
    // ── GitHub bridge (dev_team_*) ────────────────────────────────────────
    gh_open:             bool,         // whole section expanded?
    gh_repo:             String,       // "Redandan/AgoraMarket"
    gh_issue_str:        String,       // "#34" or "34" — parsed on submit
    gh_project_key:      String,       // "agora_market" / "sirin" / ...
    gh_dry_run:          bool,         // default true (set in helper)
    gh_action_msg:       Option<(bool, String)>,  // (is_error, text)
    gh_issue_preview:    Option<GhIssueView>,
    gh_previews:         Option<Vec<DryRunPreviewView>>,
    gh_expanded_preview: Option<String>,
}

/// Sensible defaults the moment the UI shows the section. We can't put these
/// in `Default` because `gh_dry_run` would be `false` by default, but we want
/// `true` as the safe starting state.
fn ensure_bridge_defaults_initialised(state: &mut TeamPanelState) {
    if state.gh_repo.is_empty() && state.gh_project_key.is_empty()
        && state.gh_issue_str.is_empty() && !state.gh_dry_run {
        state.gh_dry_run     = true;
        state.gh_repo        = "Redandan/AgoraMarket".to_string();
        state.gh_project_key = "agora_market".to_string();
    }
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

    // Token usage — backend caches 5s, safe to call every frame
    let usage = svc.team_token_usage(300);

    ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            show_header(ui, svc, &dash, state);
            ui.add_space(theme::SP_MD);
            show_member_cards(ui, svc, &dash, state);
            ui.add_space(theme::SP_MD);
            show_token_burn_card(ui, &usage);
            ui.add_space(theme::SP_MD);
            show_github_bridge_section(ui, svc, state);
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
            if !dash.worker_running
                && ui.add(
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

// ── Token Burn card ───────────────────────────────────────────────────────────

fn show_token_burn_card(ui: &mut egui::Ui, usage: &crate::ui_service::TokenUsageView) {
    egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .inner_margin(theme::SP_MD)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // Section title
            ui.label(
                RichText::new("Token Burn")
                    .size(theme::FONT_SMALL)
                    .strong()
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::SP_XS);

            // Primary metrics row: tokens/min + cost/hr
            ui.horizontal(|ui| {
                // Tokens/min (large, accent)
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(format!("{}", usage.tokens_per_min))
                            .text_style(egui::TextStyle::Monospace)
                            .size(theme::FONT_HEADING)
                            .color(theme::ACCENT),
                    );
                    ui.label(
                        RichText::new("tok/min")
                            .size(theme::FONT_CAPTION)
                            .color(theme::TEXT_DIM),
                    );
                });

                ui.add_space(theme::SP_LG);

                // Cost/hr (large, white)
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(format!("${:.2}/hr", usage.cost_per_hour))
                            .text_style(egui::TextStyle::Monospace)
                            .size(theme::FONT_HEADING)
                            .color(theme::VALUE),
                    );
                    ui.label(
                        RichText::new(format!("5-min window ({} calls)", usage.api_calls))
                            .size(theme::FONT_CAPTION)
                            .color(theme::TEXT_DIM),
                    );
                });
            });

            ui.add_space(theme::SP_SM);
            theme::thin_separator(ui);
            ui.add_space(theme::SP_XS);

            // Breakdown rows: input / output / cache_r / cache_w per min
            let rows = [
                ("input",    usage.input_per_min),
                ("output",   usage.output_per_min),
                ("cache_r",  usage.cache_r_per_min),
                ("cache_w",  usage.cache_w_per_min),
            ];
            for (label, val) in rows {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{label:<8}"))
                            .text_style(egui::TextStyle::Monospace)
                            .size(theme::FONT_CAPTION)
                            .color(theme::TEXT_DIM),
                    );
                    ui.label(
                        RichText::new(format!("{val} tok/min"))
                            .text_style(egui::TextStyle::Monospace)
                            .size(theme::FONT_CAPTION)
                            .color(theme::VALUE),
                    );
                });
            }

            ui.add_space(theme::SP_XS);

            // Cache hit %
            let hit_color = if usage.cache_hit_pct >= 50.0 {
                theme::ACCENT
            } else {
                theme::TEXT_DIM
            };
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("cache hit")
                        .text_style(egui::TextStyle::Monospace)
                        .size(theme::FONT_CAPTION)
                        .color(theme::TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!("{:.1}%", usage.cache_hit_pct))
                        .text_style(egui::TextStyle::Monospace)
                        .size(theme::FONT_CAPTION)
                        .color(hit_color),
                );
            });
        });
}

// ── GitHub Bridge section (dev_team_*) ────────────────────────────────────────
//
// Form to enqueue a real GitHub issue → Sirin Dev Team, plus a list of saved
// dry-run previews with a "Replay" button to actually post one to GitHub.
//
// Default dry_run=true; flipping it off shows a red warning (live mode WILL
// post comments / may push commits depending on what Engineer decides).

fn show_github_bridge_section(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    state: &mut TeamPanelState,
) {
    ensure_bridge_defaults_initialised(state);

    egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .inner_margin(theme::SP_MD)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // ── Header (collapsible) ──────────────────────────────────────
            ui.horizontal(|ui| {
                let arrow = if state.gh_open { "▼" } else { "▶" };
                let title = format!("{arrow}  GitHub Bridge — issue → 開發小隊");
                if ui.add(
                    egui::Button::new(
                        RichText::new(title)
                            .size(theme::FONT_SMALL)
                            .strong()
                            .color(theme::TEXT),
                    )
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::NONE),
                ).clicked() {
                    state.gh_open = !state.gh_open;
                    if state.gh_open && state.gh_previews.is_none() {
                        state.gh_previews = Some(svc.dev_team_list_previews());
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let preview_count = state.gh_previews
                        .as_ref().map(|v| v.len()).unwrap_or(0);
                    if preview_count > 0 {
                        ui.label(
                            RichText::new(format!("{preview_count} preview"))
                                .size(theme::FONT_CAPTION)
                                .color(theme::TEXT_DIM),
                        );
                    }
                });
            });

            if !state.gh_open { return; }

            ui.add_space(theme::SP_SM);
            theme::thin_separator(ui);
            ui.add_space(theme::SP_SM);

            // ── Form ──────────────────────────────────────────────────────
            show_bridge_form(ui, svc, state);

            ui.add_space(theme::SP_SM);

            // ── Issue preview (after Read button) ─────────────────────────
            if let Some(issue) = state.gh_issue_preview.clone() {
                show_issue_preview_card(ui, &issue);
                ui.add_space(theme::SP_SM);
            }

            // ── Action result toast ───────────────────────────────────────
            if let Some((is_err, msg)) = state.gh_action_msg.clone() {
                let color = if is_err { theme::DANGER } else { theme::ACCENT };
                ui.colored_label(color, RichText::new(&msg).size(theme::FONT_CAPTION));
                ui.add_space(theme::SP_SM);
            }

            // ── Preview list ──────────────────────────────────────────────
            theme::thin_separator(ui);
            ui.add_space(theme::SP_SM);
            show_preview_list(ui, svc, state);
        });
}

fn show_bridge_form(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    state: &mut TeamPanelState,
) {
    // Row 1: project_key + gh_repo + issue
    ui.horizontal(|ui| {
        ui.label(RichText::new("project").size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
        ui.add(egui::TextEdit::singleline(&mut state.gh_project_key)
            .desired_width(110.0)
            .hint_text("agora_market"));
        ui.add_space(theme::SP_SM);

        ui.label(RichText::new("repo").size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
        ui.add(egui::TextEdit::singleline(&mut state.gh_repo)
            .desired_width(220.0)
            .hint_text("Redandan/AgoraMarket"));
        ui.add_space(theme::SP_SM);

        ui.label(RichText::new("#").size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
        ui.add(egui::TextEdit::singleline(&mut state.gh_issue_str)
            .desired_width(60.0)
            .hint_text("34"));
    });

    ui.add_space(theme::SP_XS);

    // Row 2: dry-run checkbox + buttons
    ui.horizontal(|ui| {
        // dry_run checkbox — when off, label turns red
        let dry_color = if state.gh_dry_run { theme::ACCENT } else { theme::DANGER };
        ui.checkbox(&mut state.gh_dry_run, "");
        ui.colored_label(dry_color, RichText::new("DRY-RUN")
            .size(theme::FONT_CAPTION).strong());
        if !state.gh_dry_run {
            ui.colored_label(theme::DANGER,
                RichText::new("⚠ 會貼 GitHub 留言/可能 push！")
                    .size(theme::FONT_CAPTION));
        } else {
            ui.colored_label(theme::TEXT_DIM,
                RichText::new("不會碰 GitHub；review 存到 preview 檔")
                    .size(theme::FONT_CAPTION));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // ── Enqueue button ────────────────────────────────────────
            let enq_label = if state.gh_dry_run { "▶ 送進佇列 (DRY-RUN)" } else { "▶ 送進佇列 (LIVE)" };
            let enq_color = if state.gh_dry_run { theme::ACCENT } else { theme::DANGER };
            if ui.add(
                egui::Button::new(
                    RichText::new(enq_label).size(theme::FONT_SMALL).color(enq_color),
                )
                .fill(enq_color.linear_multiply(0.1))
                .stroke(egui::Stroke::new(1.0, enq_color.linear_multiply(0.4)))
                .corner_radius(4.0),
            ).clicked() {
                handle_enqueue(svc, state);
            }
            ui.add_space(theme::SP_SM);

            // ── Read issue button ─────────────────────────────────────
            if ui.add(
                egui::Button::new(
                    RichText::new("讀取 issue").size(theme::FONT_SMALL).color(theme::TEXT_DIM),
                )
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::new(0.5, theme::BORDER)),
            ).clicked() {
                handle_read_issue(svc, state);
            }
            ui.add_space(theme::SP_SM);

            // ── Refresh previews button ──────────────────────────────
            if ui.add(
                egui::Button::new(
                    RichText::new("⟳ Previews").size(theme::FONT_SMALL).color(theme::TEXT_DIM),
                )
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::new(0.5, theme::BORDER)),
            ).clicked() {
                state.gh_previews = Some(svc.dev_team_list_previews());
            }
        });
    });
}

/// Parse and submit the form to `dev_team_enqueue_issue`.
fn handle_enqueue(svc: &Arc<dyn AppService>, state: &mut TeamPanelState) {
    let issue_str = state.gh_issue_str.trim().trim_start_matches('#');
    let issue_num: u32 = match issue_str.parse() {
        Ok(n) if n >= 1 => n,
        _ => {
            state.gh_action_msg = Some((true,
                format!("issue 編號無效: '{}'", state.gh_issue_str)));
            return;
        }
    };
    let project = state.gh_project_key.trim();
    let repo    = state.gh_repo.trim();
    if project.is_empty() || repo.is_empty() {
        state.gh_action_msg = Some((true, "project / repo 不能為空".into()));
        return;
    }

    match svc.dev_team_enqueue_issue(project, repo, issue_num, state.gh_dry_run, 50) {
        Ok(task_id) => {
            let mode = if state.gh_dry_run { "DRY-RUN" } else { "LIVE" };
            state.gh_action_msg = Some((false,
                format!("✓ 已加入佇列 ({mode}) — task_id={task_id}")));
            // Refresh queue immediately so user sees it
            state.last_refresh = None;
        }
        Err(e) => {
            state.gh_action_msg = Some((true, format!("✗ 加入佇列失敗：{e}")));
        }
    }
}

fn handle_read_issue(svc: &Arc<dyn AppService>, state: &mut TeamPanelState) {
    let issue_str = state.gh_issue_str.trim().trim_start_matches('#');
    let issue_num: u32 = match issue_str.parse() {
        Ok(n) if n >= 1 => n,
        _ => {
            state.gh_action_msg = Some((true,
                format!("issue 編號無效: '{}'", state.gh_issue_str)));
            return;
        }
    };
    let repo = state.gh_repo.trim();
    if repo.is_empty() {
        state.gh_action_msg = Some((true, "repo 不能為空".into()));
        return;
    }
    match svc.dev_team_read_issue(repo, issue_num) {
        Ok(issue) => {
            state.gh_issue_preview = Some(issue);
            state.gh_action_msg = None;
        }
        Err(e) => {
            state.gh_action_msg = Some((true, format!("✗ gh issue view 失敗：{e}")));
            state.gh_issue_preview = None;
        }
    }
}

fn show_issue_preview_card(ui: &mut egui::Ui, issue: &GhIssueView) {
    egui::Frame::new()
        .fill(theme::BG)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .inner_margin(theme::SP_SM)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Title + URL
            ui.label(RichText::new(&issue.title).size(theme::FONT_SMALL).strong().color(theme::TEXT));
            ui.label(RichText::new(&issue.url).size(theme::FONT_CAPTION)
                .color(theme::INFO).underline());
            // Labels
            if !issue.labels.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for l in &issue.labels {
                        ui.label(RichText::new(format!("[{l}]"))
                            .size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
                    }
                });
            }
            ui.add_space(theme::SP_XS);
            // Body — char-boundary-safe trim to ~600 chars for the inline card
            let body_short = trunc_at_char_boundary(&issue.body, 600);
            ui.label(RichText::new(&body_short).size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
            if issue.body.len() > body_short.len() {
                ui.label(RichText::new(format!("…（截斷，原 {} chars）", issue.body.len()))
                    .size(theme::FONT_CAPTION).color(theme::TEXT_DIM).italics());
            }
        });
}

fn show_preview_list(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    state: &mut TeamPanelState,
) {
    ui.label(RichText::new("DRY-RUN Previews（人類 review → 點 Replay 真的貼到 GitHub）")
        .size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
    ui.add_space(theme::SP_XS);

    let previews = match state.gh_previews.as_ref() {
        Some(v) if !v.is_empty() => v.clone(),
        Some(_) => {
            ui.colored_label(theme::TEXT_DIM,
                RichText::new("（尚無 dry-run preview — 用上方表單跑一個 DRY-RUN 任務）")
                    .size(theme::FONT_CAPTION));
            return;
        }
        None => {
            // Lazy load on first expand
            let v = svc.dev_team_list_previews();
            state.gh_previews = Some(v.clone());
            v
        }
    };

    // Cap rendered list at 25 — older entries still on disk via MCP.
    for p in previews.iter().take(25) {
        show_preview_row(ui, svc, state, p);
        ui.add_space(theme::SP_XS);
    }
    if previews.len() > 25 {
        ui.label(RichText::new(format!("…還有 {} 筆 — 用 MCP dev_team_list_previews 看完整清單",
            previews.len() - 25))
            .size(theme::FONT_CAPTION).color(theme::TEXT_DIM).italics());
    }
}

fn show_preview_row(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    state: &mut TeamPanelState,
    p: &DryRunPreviewView,
) {
    let is_expanded = state.gh_expanded_preview.as_deref() == Some(&p.task_id);
    let stroke_color = if p.success {
        theme::ACCENT.linear_multiply(0.35)
    } else {
        theme::DANGER.linear_multiply(0.35)
    };

    egui::Frame::new()
        .fill(theme::BG)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(1.0, stroke_color))
        .inner_margin(theme::SP_SM)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let icon = if p.success { "✓" } else { "✗" };
                let icon_color = if p.success { theme::ACCENT } else { theme::DANGER };
                ui.colored_label(icon_color, icon);
                // task_id (short)
                let short_id = if p.task_id.len() > 10 { &p.task_id[..10] } else { &p.task_id };
                ui.label(RichText::new(short_id)
                    .text_style(egui::TextStyle::Monospace)
                    .size(theme::FONT_CAPTION).color(theme::TEXT));
                ui.label(RichText::new(&p.issue_url)
                    .size(theme::FONT_CAPTION).color(theme::INFO).underline());
                ui.label(RichText::new(format!("· {}", short_time(&p.saved_at)))
                    .size(theme::FONT_CAPTION).color(theme::TEXT_DIM));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Replay button
                    if ui.add(
                        egui::Button::new(
                            RichText::new("📤 Replay → GitHub")
                                .size(theme::FONT_CAPTION).color(theme::DANGER),
                        )
                        .fill(theme::DANGER.linear_multiply(0.1))
                        .stroke(egui::Stroke::new(1.0, theme::DANGER.linear_multiply(0.4)))
                        .corner_radius(4.0),
                    ).clicked() {
                        match svc.dev_team_replay_preview(&p.task_id) {
                            Ok(_) => state.gh_action_msg = Some((false,
                                format!("✓ 已貼到 {}", p.issue_url))),
                            Err(e) => state.gh_action_msg = Some((true,
                                format!("✗ Replay 失敗：{e}"))),
                        }
                    }
                    ui.add_space(theme::SP_XS);
                    // Toggle body
                    let toggle = if is_expanded { "▲ 收合" } else { "▼ 看內容" };
                    if ui.add(
                        egui::Button::new(
                            RichText::new(toggle).size(theme::FONT_CAPTION).color(theme::TEXT_DIM),
                        )
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::new(0.5, theme::BORDER)),
                    ).clicked() {
                        state.gh_expanded_preview = if is_expanded {
                            None
                        } else {
                            Some(p.task_id.clone())
                        };
                    }
                });
            });

            if is_expanded {
                ui.add_space(theme::SP_XS);
                theme::thin_separator(ui);
                ui.add_space(theme::SP_XS);
                // Body (full)
                ui.label(RichText::new(&p.body)
                    .text_style(egui::TextStyle::Monospace)
                    .size(theme::FONT_CAPTION).color(theme::TEXT));
            }
        });
}

/// Char-boundary-safe truncation for inline previews.
fn trunc_at_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes { return s.to_string(); }
    let end = (0..=max_bytes).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    s[..end].to_string()
}

/// Pull "HH:MM" out of an RFC3339 timestamp for compact display. Falls back to
/// the original string if parsing fails.
fn short_time(rfc3339: &str) -> String {
    if let Some(t_idx) = rfc3339.find('T') {
        let after_t = &rfc3339[t_idx + 1..];
        if after_t.len() >= 5 {
            return after_t[..5].to_string();   // "HH:MM"
        }
    }
    rfc3339.to_string()
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
