//! Coverage Panel — full-page 3-tier funnel + per-group + per-feature detail.
//!
//! Tiers (top to bottom in the funnel):
//!   1. Discovered — features Sirin's auto-crawler found (mock until real
//!                   discovery module ships)
//!   2. Covered    — features with at least one YAML test_id
//!   3. Scripted   — covered features whose tests are deterministic replay
//!                   scripts (no longer need LLM decisions)
//!
//! Per-feature glyphs:
//!   ⚡ = scripted (status: confirmed) — deterministic, fastest
//!   🤖 = LLM-only (status: partial)   — still requires LLM each run
//!   ○ = missing                      — no test at all

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use crate::ui_service::{AppService, CoverageData, DiscoveryStatus};
use super::theme;

// ── State ────────────────────────────────────────────────────────────────────

pub struct CoveragePanelState {
    data:    Option<Result<CoverageData, String>>,
    refresh: std::time::Instant,
}

impl Default for CoveragePanelState {
    fn default() -> Self {
        Self {
            data:    None,
            // Force immediate load on first frame.
            refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut CoveragePanelState) {
    if state.data.is_none()
        || state.refresh.elapsed() > std::time::Duration::from_secs(30)
    {
        state.data    = Some(svc.test_coverage_data());
        state.refresh = std::time::Instant::now();
    }

    // ── Header ───────────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.colored_label(
            theme::TEXT,
            RichText::new("COVERAGE").size(theme::FONT_TITLE).strong(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(
                RichText::new("↻ Refresh").size(theme::FONT_CAPTION).color(theme::TEXT_DIM)
            ).frame(false)).clicked() {
                state.data    = Some(svc.test_coverage_data());
                state.refresh = std::time::Instant::now();
            }
        });
    });
    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    ScrollArea::vertical().id_salt("coverage_panel").show(ui, |ui| {
        match &state.data {
            None => {
                ui.centered_and_justified(|ui| {
                    ui.colored_label(theme::TEXT_DIM, "Loading…");
                });
            }
            Some(Err(e)) => {
                ui.colored_label(
                    theme::DANGER,
                    RichText::new(format!("⚠ {e}")).size(theme::FONT_SMALL),
                );
            }
            Some(Ok(data)) => {
                show_funnel(ui, data);
                ui.add_space(theme::SP_LG);
                show_groups(ui, data);
                show_gaps(ui, data);
            }
        }
    });
}

// ── 3-tier funnel (top of page) ──────────────────────────────────────────────

fn show_funnel(ui: &mut egui::Ui, d: &CoverageData) {
    let total = d.discovered.max(1);
    let cov_pct = d.total_covered as f32 / total as f32;
    let scr_pct = d.scripted as f32 / total as f32;
    let bar_w = 300.0;

    egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .inner_margin(theme::SP_MD)
        .show(ui, |ui| {
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
            });
            ui.add_space(theme::SP_SM);

            funnel_row(ui, "探索 Discovered",
                d.discovered, total, 1.0, theme::TEXT_DIM, bar_w);
            if matches!(d.discovery_status, DiscoveryStatus::NotRun) {
                ui.horizontal(|ui| {
                    ui.add_space(160.0);
                    ui.colored_label(
                        theme::TEXT_DIM,
                        RichText::new("(mock — discovery 模組待實作；功能規劃中)")
                            .size(theme::FONT_CAPTION).italics(),
                    );
                });
            }
            funnel_row(ui, "覆蓋 Covered",
                d.total_covered, total, cov_pct, theme::ACCENT, bar_w);
            funnel_row(ui, "有腳本 Scripted",
                d.scripted, total, scr_pct, theme::INFO, bar_w);

            ui.add_space(theme::SP_SM);
            ui.horizontal(|ui| {
                ui.add_space(160.0);
                ui.colored_label(
                    theme::TEXT_DIM,
                    RichText::new("⚡ scripted   🤖 LLM-only   ○ missing")
                        .size(theme::FONT_CAPTION).monospace(),
                );
            });
        });
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
            egui::vec2(160.0, 18.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.colored_label(theme::TEXT,
                    RichText::new(label).size(theme::FONT_SMALL));
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(40.0, 18.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.colored_label(theme::TEXT,
                    RichText::new(format!("{n}"))
                        .size(theme::FONT_SMALL).monospace().strong());
            },
        );
        ui.add_space(theme::SP_SM);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(bar_w, 8.0), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 2.0, theme::BORDER);
        let fw = (rect.width() * pct.clamp(0.0, 1.0)).max(0.0);
        p.rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(fw, 8.0)),
            2.0, color,
        );
        ui.add_space(theme::SP_SM);
        ui.colored_label(theme::TEXT_DIM,
            RichText::new(format!("{:.0}%", pct * 100.0))
                .size(theme::FONT_CAPTION).monospace());
    });
    ui.add_space(theme::SP_XS);
}

// ── Per-group cards ──────────────────────────────────────────────────────────

fn show_groups(ui: &mut egui::Ui, d: &CoverageData) {
    for group in &d.groups {
        let gpct = if group.total > 0 {
            group.covered as f32 / group.total as f32
        } else { 0.0 };
        let gcol = coverage_color(gpct);

        egui::Frame::new()
            .fill(theme::CARD)
            .corner_radius(4.0)
            .inner_margin(egui::vec2(theme::SP_MD, theme::SP_SM))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(gcol, RichText::new("●").size(9.0));
                    ui.colored_label(theme::TEXT,
                        RichText::new(&group.name)
                            .size(theme::FONT_SMALL).strong());
                    if !group.role.is_empty() {
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new(format!("[{}]", group.role))
                                .size(theme::FONT_CAPTION).monospace());
                    }
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.colored_label(gcol,
                                RichText::new(format!("{}/{}", group.covered, group.total))
                                    .size(theme::FONT_CAPTION).monospace());
                            let bw = 60.0; let bh = 6.0;
                            let (r, _) = ui.allocate_exact_size(
                                egui::vec2(bw, bh), egui::Sense::hover());
                            let p = ui.painter_at(r);
                            p.rect_filled(r, 2.0, theme::BORDER);
                            let fw = (r.width() * gpct).max(0.0);
                            p.rect_filled(egui::Rect::from_min_size(
                                r.min, egui::vec2(fw, bh)), 2.0, gcol);
                        },
                    );
                });
                ui.add_space(theme::SP_XS);
                for feat in &group.features {
                    let (glyph, col) = match feat.status.as_str() {
                        "confirmed" => ("⚡", theme::INFO),
                        "partial"   => ("🤖", theme::YELLOW),
                        _           => ("○",  theme::DANGER),
                    };
                    ui.horizontal(|ui| {
                        ui.add_space(theme::SP_MD);
                        ui.colored_label(col,
                            RichText::new(glyph).size(theme::FONT_BODY));
                        ui.colored_label(
                            if feat.status == "missing" { theme::TEXT_DIM } else { theme::TEXT },
                            RichText::new(&feat.name).size(theme::FONT_CAPTION),
                        );
                        if !feat.test_ids.is_empty() {
                            ui.colored_label(theme::TEXT_DIM,
                                RichText::new(format!("← {}", feat.test_ids.join(", ")))
                                    .size(theme::FONT_CAPTION).monospace());
                        }
                    });
                }
            });
        ui.add_space(theme::SP_XS);
    }
}

// ── Bottom GAPS list ─────────────────────────────────────────────────────────

fn show_gaps(ui: &mut egui::Ui, d: &CoverageData) {
    let mut coverage_gaps: Vec<(&str, &str, &str)> = Vec::new();
    for group in &d.groups {
        for feat in &group.features {
            if feat.status == "missing" {
                coverage_gaps.push((&group.name, &feat.id, &feat.name));
            }
        }
    }

    if coverage_gaps.is_empty() {
        return;
    }

    ui.add_space(theme::SP_SM);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    ui.colored_label(theme::DANGER,
        RichText::new(format!("COVERAGE GAPS  {} missing", coverage_gaps.len()))
            .size(theme::FONT_SMALL).strong());
    ui.add_space(theme::SP_XS);
    for (gname, fid, fname) in &coverage_gaps {
        ui.horizontal(|ui| {
            ui.add_space(theme::SP_SM);
            ui.colored_label(theme::DANGER,
                RichText::new("○").size(theme::FONT_CAPTION));
            ui.colored_label(theme::TEXT_DIM,
                RichText::new(*fname).size(theme::FONT_CAPTION));
            ui.colored_label(theme::TEXT_DIM,
                RichText::new(format!("— {gname}")).size(theme::FONT_CAPTION));
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.colored_label(theme::INFO,
                        RichText::new(format!("agora_{fid}"))
                            .size(theme::FONT_CAPTION).monospace())
                        .on_hover_text("suggested YAML test name");
                },
            );
        });
    }

    // Discovery gaps (mock — empty until real crawler ships).
    ui.add_space(theme::SP_SM);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);
    ui.colored_label(theme::TEXT_DIM,
        RichText::new("DISCOVERY GAPS  0 (discovery module 待實作)")
            .size(theme::FONT_SMALL).strong());
    ui.add_space(theme::SP_XS);
    ui.horizontal(|ui| {
        ui.add_space(theme::SP_SM);
        ui.colored_label(theme::TEXT_DIM,
            RichText::new("(自動爬到、但 YAML 沒列的功能會出現在這裡)")
                .size(theme::FONT_CAPTION).italics());
    });
}

fn coverage_color(pct: f32) -> egui::Color32 {
    if pct >= 0.80 { theme::ACCENT }
    else if pct >= 0.50 { theme::YELLOW }
    else { theme::DANGER }
}
