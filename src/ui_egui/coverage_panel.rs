//! Coverage Panel — standalone view (previously sub-tab of test_dashboard).
//! Shows the feature map from config/coverage/agora_market.yaml with
//! per-group progress bars and a missing-feature gap list.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use crate::ui_service::{AppService, CoverageData};
use super::theme;

// ── State ──────────────────────────────────────────────────────��─────────────

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
    // Refresh every 30 s or on first load.
    if state.data.is_none()
        || state.refresh.elapsed() > std::time::Duration::from_secs(30)
    {
        state.data    = Some(svc.test_coverage_data());
        state.refresh = std::time::Instant::now();
    }

    // Header row with refresh button.
    ui.horizontal(|ui| {
        ui.colored_label(theme::TEXT, RichText::new("COVERAGE").size(theme::FONT_TITLE).strong());
        ui.add_space(theme::SP_SM);
        if let Some(Ok(data)) = &state.data {
            let pct = if data.total_features > 0 {
                data.total_covered as f32 / data.total_features as f32
            } else { 0.0 };
            let col = coverage_color(pct);
            ui.colored_label(col,
                RichText::new(format!("{}/{} ({:.0}%)",
                    data.total_covered, data.total_features, pct * 100.0))
                    .size(theme::FONT_SMALL).strong().monospace(),
            );
        }
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
                ui.colored_label(theme::DANGER,
                    RichText::new(format!("⚠ {e}")).size(theme::FONT_SMALL));
            }
            Some(Ok(data)) => {
                show_coverage_data(ui, data);
            }
        }
    });
}

// ── Internal renderers ────────────────────────────────────────────────────────

fn show_coverage_data(ui: &mut egui::Ui, data: &CoverageData) {
    let pct = if data.total_features > 0 {
        data.total_covered as f32 / data.total_features as f32
    } else { 0.0 };

    // Overall bar.
    ui.horizontal(|ui| {
        ui.colored_label(theme::TEXT_DIM,
            RichText::new(format!("{} v{}", data.product, data.version))
                .size(theme::FONT_CAPTION).monospace());
        ui.add_space(theme::SP_MD);
        let bar_w = 160.0;
        let bar_h = 8.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, bar_h), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 3.0, theme::BORDER);
        let fill_w = (rect.width() * pct).max(0.0);
        p.rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, bar_h)),
            3.0, coverage_color(pct));
    });
    ui.add_space(theme::SP_MD);

    let mut gaps: Vec<(&str, &str, &str)> = Vec::new();

    for group in &data.groups {
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
                        RichText::new(&group.name).size(theme::FONT_SMALL).strong());
                    if !group.role.is_empty() {
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new(format!("[{}]", group.role))
                                .size(theme::FONT_CAPTION).monospace());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(gcol,
                            RichText::new(format!("{}/{}", group.covered, group.total))
                                .size(theme::FONT_CAPTION).monospace());
                        let bw = 60.0; let bh = 6.0;
                        let (r, _) = ui.allocate_exact_size(egui::vec2(bw, bh), egui::Sense::hover());
                        let p = ui.painter_at(r);
                        p.rect_filled(r, 2.0, theme::BORDER);
                        let fw = (r.width() * gpct).max(0.0);
                        p.rect_filled(egui::Rect::from_min_size(r.min, egui::vec2(fw, bh)), 2.0, gcol);
                    });
                });
                ui.add_space(theme::SP_XS);
                for feat in &group.features {
                    let (dot, col) = match feat.status.as_str() {
                        "confirmed" => ("✓", theme::ACCENT),
                        "partial"   => ("◐", theme::YELLOW),
                        _           => ("○", theme::DANGER),
                    };
                    ui.horizontal(|ui| {
                        ui.add_space(theme::SP_MD);
                        ui.colored_label(col, RichText::new(dot).size(theme::FONT_CAPTION));
                        ui.colored_label(
                            if feat.status == "missing" { theme::TEXT_DIM } else { theme::TEXT },
                            RichText::new(&feat.name).size(theme::FONT_CAPTION));
                        if !feat.test_ids.is_empty() {
                            ui.colored_label(theme::TEXT_DIM,
                                RichText::new(format!("← {}", feat.test_ids.join(", ")))
                                    .size(theme::FONT_CAPTION).monospace());
                        }
                    });
                    if feat.status == "missing" {
                        gaps.push((&group.name, &feat.id, &feat.name));
                    }
                }
            });
        ui.add_space(theme::SP_XS);
    }

    if !gaps.is_empty() {
        ui.add_space(theme::SP_SM);
        theme::thin_separator(ui);
        ui.add_space(theme::SP_SM);
        ui.colored_label(theme::DANGER,
            RichText::new(format!("GAPS  {} missing", gaps.len()))
                .size(theme::FONT_SMALL).strong());
        ui.add_space(theme::SP_XS);
        for (gname, fid, fname) in &gaps {
            ui.horizontal(|ui| {
                ui.add_space(theme::SP_SM);
                ui.colored_label(theme::DANGER, RichText::new("○").size(theme::FONT_CAPTION));
                ui.colored_label(theme::TEXT_DIM,
                    RichText::new(*fname).size(theme::FONT_CAPTION));
                ui.colored_label(theme::TEXT_DIM,
                    RichText::new(format!("— {gname}")).size(theme::FONT_CAPTION));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.colored_label(theme::INFO,
                        RichText::new(format!("agora_{fid}"))
                            .size(theme::FONT_CAPTION).monospace())
                        .on_hover_text("suggested YAML test name");
                });
            });
        }
    }
}

pub fn coverage_color(pct: f32) -> egui::Color32 {
    if pct >= 0.80 { theme::ACCENT }
    else if pct >= 0.50 { theme::YELLOW }
    else { theme::DANGER }
}
