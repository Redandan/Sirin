//! Dashboard — main landing view (default after launch).
//!
//! Sections (top to bottom):
//!   • ACTIVE      currently running tests (max 3)
//!   • RECENT      last 8 completed runs
//!   • COVERAGE    3-tier funnel card (Discovered / Covered / Scripted)
//!   • BROWSER     status + URL + title
//!
//! Clicking any card / row signals a `DashboardAction` which the parent
//! (mod.rs) translates into a view switch — usually `View::Testing` with the
//! correct internal tab pre-selected.

use std::sync::Arc;
use eframe::egui::{self, RichText};
use crate::ui_service::{AppService, CoverageData, DiscoveryStatus, TestRunView};
use super::theme;

#[derive(Default, PartialEq, Clone, Copy)]
pub enum DashboardAction {
    #[default]
    None,
    OpenTesting,
    OpenTestingCoverage,
    OpenTestingBrowser,
}

pub struct DashboardState {
    coverage:         Option<Result<CoverageData, String>>,
    coverage_refresh: std::time::Instant,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            coverage:         None,
            coverage_refresh: std::time::Instant::now() - std::time::Duration::from_secs(120),
        }
    }
}

pub fn show(
    ui:    &mut egui::Ui,
    svc:   &Arc<dyn AppService>,
    state: &mut DashboardState,
) -> DashboardAction {
    let mut action = DashboardAction::None;

    // Refresh coverage every 60 s (cheap — file read + YAML parse).
    if state.coverage.is_none()
        || state.coverage_refresh.elapsed() > std::time::Duration::from_secs(60)
    {
        state.coverage         = Some(svc.test_coverage_data());
        state.coverage_refresh = std::time::Instant::now();
    }

    // ── Header row ───────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("DASHBOARD")
                .size(theme::FONT_TITLE).strong().color(theme::TEXT),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let btn = egui::Button::new(
                RichText::new("▶ Run Test")
                    .size(theme::FONT_SMALL).color(theme::BG).strong(),
            )
            .fill(theme::ACCENT)
            .corner_radius(4.0);
            if ui.add(btn).clicked() {
                action = DashboardAction::OpenTesting;
            }
        });
    });
    ui.add_space(theme::SP_SM);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_LG);

    egui::ScrollArea::vertical().id_salt("dashboard_scroll").show(ui, |ui| {
        // ── ACTIVE ───────────────────────────────────────────────────────
        let active = svc.active_test_runs();
        section_header(ui, &format!("ACTIVE  ({})", active.len()));
        if active.is_empty() {
            empty_card(ui, "目前沒有正在執行的測試");
        } else {
            for run in active.iter().take(3) {
                if active_run_card(ui, run).clicked() {
                    action = DashboardAction::OpenTesting;
                }
            }
        }
        ui.add_space(theme::SP_LG);

        // ── RECENT ───────────────────────────────────────────────────────
        let recent = svc.recent_test_runs(8);
        section_header(ui, &format!("RECENT  ({} of last 8)", recent.len()));
        if recent.is_empty() {
            empty_card(ui, "尚無歷史紀錄");
        } else {
            for run in &recent {
                if recent_run_row(ui, run).clicked() {
                    action = DashboardAction::OpenTesting;
                }
            }
        }
        ui.add_space(theme::SP_LG);

        // ── COVERAGE 3-tier funnel ───────────────────────────────────────
        section_header(ui, "COVERAGE");
        if coverage_card(ui, state.coverage.as_ref()).clicked() {
            action = DashboardAction::OpenTestingCoverage;
        }
        ui.add_space(theme::SP_LG);

        // ── BROWSER ──────────────────────────────────────────────────────
        section_header(ui, "BROWSER");
        if browser_card(ui, svc).clicked() {
            action = DashboardAction::OpenTestingBrowser;
        }
    });

    action
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.colored_label(
        theme::TEXT_DIM,
        RichText::new(text)
            .size(theme::FONT_SMALL).strong().monospace(),
    );
    ui.add_space(theme::SP_XS);
}

fn empty_card(ui: &mut egui::Ui, msg: &str) {
    egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .inner_margin(theme::SP_MD)
        .show(ui, |ui| {
            ui.colored_label(
                theme::TEXT_DIM,
                RichText::new(msg).size(theme::FONT_CAPTION),
            );
        });
}

fn active_run_card(ui: &mut egui::Ui, run: &TestRunView) -> egui::Response {
    let inner = egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .inner_margin(theme::SP_MD)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(
                    theme::INFO,
                    RichText::new("▶").size(theme::FONT_BODY).strong(),
                );
                ui.colored_label(
                    theme::TEXT,
                    RichText::new(&run.test_id)
                        .size(theme::FONT_BODY).monospace().strong(),
                );
                if let Some(step) = run.step {
                    ui.colored_label(
                        theme::TEXT_DIM,
                        RichText::new(format!("· step {step}"))
                            .size(theme::FONT_CAPTION).monospace(),
                    );
                }
            });
            if let Some(an) = &run.analysis {
                let trunc = truncate(an, 110);
                ui.add_space(2.0);
                ui.colored_label(
                    theme::TEXT_DIM,
                    RichText::new(trunc).size(theme::FONT_CAPTION),
                );
            }
        });
    ui.add_space(theme::SP_XS);
    inner.response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn recent_run_row(ui: &mut egui::Ui, run: &TestRunView) -> egui::Response {
    let (col, sym) = match run.status.as_str() {
        "passed"  => (theme::ACCENT,   "✓"),
        "failed"  => (theme::DANGER,   "✗"),
        "timeout" => (theme::YELLOW,   "⌚"),
        "error"   => (theme::DANGER,   "!"),
        _         => (theme::TEXT_DIM, "·"),
    };
    let inner = egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(3.0)
        .inner_margin(egui::vec2(theme::SP_MD, theme::SP_XS))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(
                    col,
                    RichText::new(sym).size(theme::FONT_BODY).strong(),
                );
                ui.colored_label(
                    col,
                    RichText::new(run.status.to_uppercase())
                        .size(theme::FONT_CAPTION).monospace().strong(),
                );
                ui.add_space(theme::SP_SM);
                ui.colored_label(
                    theme::TEXT,
                    RichText::new(&run.test_id)
                        .size(theme::FONT_SMALL).monospace(),
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if let Some(d) = run.duration_ms {
                            ui.colored_label(
                                theme::TEXT_DIM,
                                RichText::new(format!("{:.1}s", d as f32 / 1000.0))
                                    .size(theme::FONT_CAPTION).monospace(),
                            );
                        }
                        if let Some(rate) = run.pass_rate {
                            if rate < 0.7 {
                                ui.colored_label(
                                    theme::YELLOW,
                                    RichText::new(format!("· flaky {:.0}%", rate * 100.0))
                                        .size(theme::FONT_CAPTION).monospace(),
                                );
                            }
                        }
                    },
                );
            });
        });
    ui.add_space(2.0);
    inner.response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn coverage_card(
    ui:   &mut egui::Ui,
    data: Option<&Result<CoverageData, String>>,
) -> egui::Response {
    let inner = egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .inner_margin(theme::SP_MD)
        .show(ui, |ui| {
            match data {
                None => {
                    ui.colored_label(theme::TEXT_DIM, "Loading…");
                }
                Some(Err(e)) => {
                    ui.colored_label(
                        theme::DANGER,
                        RichText::new(format!("⚠ {e}"))
                            .size(theme::FONT_SMALL),
                    );
                }
                Some(Ok(d)) => render_coverage_funnel(ui, d),
            }
        });
    inner.response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn render_coverage_funnel(ui: &mut egui::Ui, d: &CoverageData) {
    // Funnel denominator must accommodate the largest tier — discovered may
    // legitimately be smaller than covered (e.g., crawler only walked one
    // route but YAML covers all roles). Otherwise percentages exceed 100 %.
    let total = d.discovered.max(d.total_features).max(d.total_covered).max(d.scripted).max(1);
    let dis_pct = d.discovered     as f32 / total as f32;
    let cov_pct = d.total_covered  as f32 / total as f32;
    let scr_pct = d.scripted       as f32 / total as f32;
    let bar_w = 220.0;

    // Header row: product · version · open hint
    ui.horizontal(|ui| {
        ui.colored_label(
            theme::TEXT,
            RichText::new(&d.product).size(theme::FONT_BODY).strong(),
        );
        ui.colored_label(
            theme::TEXT_DIM,
            RichText::new(format!("v{}", d.version))
                .size(theme::FONT_CAPTION).monospace(),
        );
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.colored_label(
                    theme::INFO,
                    RichText::new("Open full →").size(theme::FONT_CAPTION),
                );
            },
        );
    });
    ui.add_space(theme::SP_SM);

    funnel_row(ui, "探索 Discovered", d.discovered, total, dis_pct, theme::TEXT_DIM, bar_w);
    if matches!(d.discovery_status, DiscoveryStatus::NotRun) {
        ui.horizontal(|ui| {
            ui.add_space(140.0);
            ui.colored_label(
                theme::TEXT_DIM,
                RichText::new("(mock — discovery 模組待實作)")
                    .size(theme::FONT_CAPTION).italics(),
            );
        });
    }
    funnel_row(ui, "覆蓋 Covered",   d.total_covered, total, cov_pct, theme::ACCENT,   bar_w);
    funnel_row(ui, "有腳本 Scripted", d.scripted,      total, scr_pct, theme::INFO,     bar_w);
}

fn funnel_row(
    ui:    &mut egui::Ui,
    label: &str,
    n:     usize,
    _max:  usize,
    pct:   f32,
    color: egui::Color32,
    bar_w: f32,
) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(140.0, 18.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.colored_label(
                    theme::TEXT,
                    RichText::new(label).size(theme::FONT_SMALL),
                );
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(40.0, 18.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.colored_label(
                    theme::TEXT,
                    RichText::new(format!("{n}"))
                        .size(theme::FONT_SMALL).monospace().strong(),
                );
            },
        );
        ui.add_space(theme::SP_SM);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(bar_w, 8.0),
            egui::Sense::hover(),
        );
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 2.0, theme::BORDER);
        let fw = (rect.width() * pct.clamp(0.0, 1.0)).max(0.0);
        p.rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(fw, 8.0)),
            2.0,
            color,
        );
        ui.add_space(theme::SP_SM);
        ui.colored_label(
            theme::TEXT_DIM,
            RichText::new(format!("{:.0}%", pct * 100.0))
                .size(theme::FONT_CAPTION).monospace(),
        );
    });
    ui.add_space(theme::SP_XS);
}

fn browser_card(ui: &mut egui::Ui, svc: &Arc<dyn AppService>) -> egui::Response {
    let inner = egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .inner_margin(theme::SP_MD)
        .show(ui, |ui| {
            let open  = svc.browser_is_open();
            let url   = svc.browser_url();
            let title = svc.browser_title();
            ui.horizontal(|ui| {
                let (col, txt) = if open {
                    (theme::ACCENT, "● LIVE")
                } else {
                    (theme::TEXT_DIM, "○ OFF")
                };
                ui.colored_label(
                    col,
                    RichText::new(txt)
                        .size(theme::FONT_SMALL).monospace().strong(),
                );
                ui.add_space(theme::SP_SM);
                if let Some(t) = &title {
                    ui.colored_label(
                        theme::TEXT,
                        RichText::new(t).size(theme::FONT_SMALL),
                    );
                }
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.colored_label(
                            theme::INFO,
                            RichText::new("Open monitor →").size(theme::FONT_CAPTION),
                        );
                    },
                );
            });
            if let Some(u) = &url {
                ui.add_space(2.0);
                ui.colored_label(
                    theme::TEXT_DIM,
                    RichText::new(u)
                        .size(theme::FONT_CAPTION).monospace(),
                );
            }
        });
    inner.response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}
