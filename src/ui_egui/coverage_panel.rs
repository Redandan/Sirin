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
use crate::ui_service::{AppService, CoverageData, DiscoveryDataView, DiscoveryStatus};
use super::theme;

// ── State ────────────────────────────────────────────────────────────────────

pub struct CoveragePanelState {
    data:           Option<Result<CoverageData, String>>,
    discovery:      Option<Result<DiscoveryDataView, String>>,
    refresh:        std::time::Instant,
    discovery_seed: String,
    /// Sticky launch toast — text + when shown — auto-clears after 6 s.
    last_launch:    Option<(String, std::time::Instant)>,
}

impl Default for CoveragePanelState {
    fn default() -> Self {
        Self {
            data:           None,
            discovery:      None,
            // Force immediate load on first frame.
            refresh:        std::time::Instant::now() - std::time::Duration::from_secs(60),
            discovery_seed: "https://redandan.github.io/?__test_role=buyer".to_string(),
            last_launch:    None,
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut CoveragePanelState) {
    // Refresh cadence: faster when crawling so progress updates live.
    let crawling = matches!(
        state.data.as_ref().and_then(|r| r.as_ref().ok()).map(|d| &d.discovery_status),
        Some(DiscoveryStatus::Crawling { .. })
    );
    let refresh_secs = if crawling { 2 } else { 30 };
    if state.data.is_none()
        || state.refresh.elapsed() > std::time::Duration::from_secs(refresh_secs)
    {
        state.data      = Some(svc.test_coverage_data());
        state.discovery = Some(svc.discovery_data());
        state.refresh   = std::time::Instant::now();
    }
    if crawling {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(2));
    }

    // Auto-clear sticky launch toast after 6 s.
    if let Some((_, t)) = &state.last_launch {
        if t.elapsed() > std::time::Duration::from_secs(6) {
            state.last_launch = None;
        }
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
                state.data      = Some(svc.test_coverage_data());
                state.discovery = Some(svc.discovery_data());
                state.refresh   = std::time::Instant::now();
            }
        });
    });
    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    // ── Discovery launcher row ───────────────────────────────────────────
    show_discovery_launcher(ui, svc, state, crawling);
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
                show_gaps(ui, data, state.discovery.as_ref());
            }
        }
    });
}

// ── Discovery launcher (URL input + Discover button + status) ───────────────

fn show_discovery_launcher(
    ui:       &mut egui::Ui,
    svc:      &Arc<dyn AppService>,
    state:    &mut CoveragePanelState,
    crawling: bool,
) {
    egui::Frame::new()
        .fill(theme::BG)
        .corner_radius(4.0)
        .inner_margin(egui::vec2(theme::SP_MD, theme::SP_XS))
        .stroke(egui::Stroke::new(0.5, theme::BORDER))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(
                    theme::TEXT_DIM,
                    RichText::new("DISCOVERY")
                        .size(theme::FONT_CAPTION).strong().monospace(),
                );
                ui.add_space(theme::SP_SM);

                // Seed URL input
                ui.add(
                    egui::TextEdit::singleline(&mut state.discovery_seed)
                        .desired_width(300.0)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("seed URL…"),
                );
                ui.add_space(theme::SP_SM);

                let btn_label = if crawling { "⌛ Crawling…" } else { "↻ Discover" };
                let btn = egui::Button::new(
                    RichText::new(btn_label)
                        .size(theme::FONT_SMALL).strong()
                        .color(if crawling { theme::TEXT_DIM } else { theme::BG }),
                )
                .fill(if crawling { theme::CARD } else { theme::ACCENT })
                .corner_radius(4.0);
                let enabled = !crawling && !state.discovery_seed.is_empty();
                if ui.add_enabled(enabled, btn).clicked() {
                    match svc.launch_discovery(&state.discovery_seed, 1) {
                        Ok(rid) => {
                            state.last_launch = Some((
                                format!("✓ launched {rid}"),
                                std::time::Instant::now(),
                            ));
                            // Force quick refresh so the funnel shows
                            // Crawling state on the next frame.
                            state.refresh = std::time::Instant::now()
                                - std::time::Duration::from_secs(60);
                        }
                        Err(e) => {
                            state.last_launch = Some((
                                format!("✗ {e}"),
                                std::time::Instant::now(),
                            ));
                        }
                    }
                }

                // Status hint (right side)
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if let Some(Ok(d)) = state.discovery.as_ref() {
                            match &d.status {
                                DiscoveryStatus::NotRun => {
                                    ui.colored_label(
                                        theme::TEXT_DIM,
                                        RichText::new("never run")
                                            .size(theme::FONT_CAPTION).monospace(),
                                    );
                                }
                                DiscoveryStatus::Crawling { started_at } => {
                                    ui.colored_label(
                                        theme::INFO,
                                        RichText::new(format!("crawling since {}",
                                            short_time(started_at)))
                                            .size(theme::FONT_CAPTION).monospace(),
                                    );
                                }
                                DiscoveryStatus::Done { at, total_widgets } => {
                                    ui.colored_label(
                                        theme::ACCENT,
                                        RichText::new(format!("✓ {} widgets · {}",
                                            total_widgets, short_time(at)))
                                            .size(theme::FONT_CAPTION).monospace(),
                                    );
                                }
                            }
                        }
                    },
                );
            });

            // Sticky launch toast
            if let Some((msg, _)) = &state.last_launch {
                ui.add_space(theme::SP_XS);
                let col = if msg.starts_with('✓') { theme::ACCENT } else { theme::DANGER };
                ui.colored_label(col,
                    RichText::new(msg).size(theme::FONT_CAPTION).monospace());
            }
        });
}

/// Render an RFC-3339 timestamp as just "HH:MM:SS" for compact UI strings.
fn short_time(rfc3339: &str) -> &str {
    // Extract the time portion between 'T' and '.' / 'Z' / '+'.
    if let Some(t_idx) = rfc3339.find('T') {
        let tail = &rfc3339[t_idx + 1..];
        let end = tail.find(|c: char| c == '.' || c == 'Z' || c == '+' || c == '-')
            .unwrap_or(tail.len());
        &tail[..end]
    } else {
        rfc3339
    }
}

// ── 3-tier funnel (top of page) ──────────────────────────────────────────────

fn show_funnel(ui: &mut egui::Ui, d: &CoverageData) {
    let total = d.discovered.max(d.total_features).max(d.total_covered).max(d.scripted).max(1);
    let dis_pct = d.discovered    as f32 / total as f32;
    let cov_pct = d.total_covered as f32 / total as f32;
    let scr_pct = d.scripted      as f32 / total as f32;
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
                d.discovered, total, dis_pct, theme::TEXT_DIM, bar_w);
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

fn show_gaps(
    ui:        &mut egui::Ui,
    d:         &CoverageData,
    discovery: Option<&Result<DiscoveryDataView, String>>,
) {
    let mut coverage_gaps: Vec<(&str, &str, &str)> = Vec::new();
    for group in &d.groups {
        for feat in &group.features {
            if feat.status == "missing" {
                coverage_gaps.push((&group.name, &feat.id, &feat.name));
            }
        }
    }

    if !coverage_gaps.is_empty() {
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
    }

    // Discovery gaps — features the crawler saw that don't match any
    // tracked YAML feature label. Heuristic: case-insensitive label
    // substring match in either direction; common UI chrome filtered out.
    ui.add_space(theme::SP_SM);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);
    match discovery {
        Some(Ok(disc)) if disc.total > 0 => {
            let gaps = discovery_gaps(&disc.features, d);
            if gaps.is_empty() {
                ui.colored_label(theme::ACCENT,
                    RichText::new(format!(
                        "DISCOVERY GAPS  0  (all {} discovered widgets match YAML)",
                        disc.total
                    )).size(theme::FONT_SMALL).strong());
            } else {
                ui.colored_label(theme::YELLOW,
                    RichText::new(format!(
                        "DISCOVERY GAPS  {} of {} (爬到但 YAML 沒列)",
                        gaps.len(), disc.total,
                    )).size(theme::FONT_SMALL).strong());
                ui.add_space(theme::SP_XS);
                for g in gaps.iter().take(50) {
                    ui.horizontal(|ui| {
                        ui.add_space(theme::SP_SM);
                        ui.colored_label(theme::YELLOW,
                            RichText::new("◆").size(theme::FONT_CAPTION));
                        ui.colored_label(theme::TEXT,
                            RichText::new(&g.label).size(theme::FONT_CAPTION));
                        ui.colored_label(theme::TEXT_DIM,
                            RichText::new(format!("[{}]", g.kind))
                                .size(theme::FONT_CAPTION).monospace());
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.colored_label(theme::TEXT_DIM,
                                    RichText::new(&g.route)
                                        .size(theme::FONT_CAPTION).monospace());
                            },
                        );
                    });
                }
                if gaps.len() > 50 {
                    ui.add_space(theme::SP_XS);
                    ui.colored_label(theme::TEXT_DIM,
                        RichText::new(format!("…and {} more (use MCP discovery_features for full list)",
                            gaps.len() - 50)).size(theme::FONT_CAPTION).italics());
                }
            }
        }
        Some(Ok(_)) => {
            ui.colored_label(theme::TEXT_DIM,
                RichText::new("DISCOVERY GAPS  —")
                    .size(theme::FONT_SMALL).strong());
            ui.add_space(theme::SP_XS);
            ui.horizontal(|ui| {
                ui.add_space(theme::SP_SM);
                ui.colored_label(theme::TEXT_DIM,
                    RichText::new("(尚未跑過 discovery — 點上方 ↻ Discover 啟動)")
                        .size(theme::FONT_CAPTION).italics());
            });
        }
        Some(Err(e)) => {
            ui.colored_label(theme::DANGER,
                RichText::new(format!("DISCOVERY GAPS  ⚠ {e}"))
                    .size(theme::FONT_SMALL).strong());
        }
        None => {
            ui.colored_label(theme::TEXT_DIM,
                RichText::new("DISCOVERY GAPS  Loading…")
                    .size(theme::FONT_SMALL).strong());
        }
    }
}

// ── Discovery gaps diff ──────────────────────────────────────────────────────

/// Diff discovered features against YAML coverage map. Returns features
/// the crawler saw whose label doesn't appear (substring, case-insensitive)
/// in any YAML feature name. Common UI chrome (Back / Close / Menu / OK …)
/// is filtered out so the list focuses on app-specific widgets.
fn discovery_gaps<'a>(
    discovered: &'a [crate::ui_service::DiscoveredFeatureView],
    coverage:   &CoverageData,
) -> Vec<&'a crate::ui_service::DiscoveredFeatureView> {
    use std::collections::HashSet;
    let yaml_labels: HashSet<String> = coverage.groups.iter()
        .flat_map(|g| g.features.iter())
        .map(|f| f.name.to_lowercase())
        .collect();
    discovered.iter().filter(|d| {
        let dl = d.label.to_lowercase();
        if is_ui_chrome(&dl) { return false; }
        !yaml_labels.iter().any(|y| {
            // Two-way substring match — handles partial overlaps both ways.
            y.contains(&dl) || (!dl.is_empty() && dl.len() > 1 && y.split_whitespace().any(|w| w == dl))
                || dl.contains(y.as_str())
        })
    }).collect()
}

/// Common UI chrome labels not specific to product features.
/// Filtered out of Discovery Gaps to reduce noise.
fn is_ui_chrome(label: &str) -> bool {
    matches!(label,
        "back" | "close" | "menu" | "home" | "skip" | "ok" | "cancel"
        | "next" | "previous" | "submit" | "save" | "delete" | "edit"
        | "more" | "search" | "settings" | "login" | "logout" | "register"
        | "返回" | "關閉" | "選單" | "首頁" | "跳過" | "確定" | "取消"
        | "下一步" | "上一步" | "送出" | "儲存" | "刪除" | "編輯"
        | "更多" | "搜尋" | "設定" | "登入" | "登出" | "註冊"
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_service::{
        CoverageData, CoverageFeatureView, CoverageGroupView,
        DiscoveredFeatureView, DiscoveryStatus,
    };

    fn mk_coverage(features: &[(&str, &str, &str)]) -> CoverageData {
        // features: list of (group, feature_name, status)
        let mut groups_map: std::collections::HashMap<String, Vec<CoverageFeatureView>> =
            std::collections::HashMap::new();
        for (g, n, s) in features {
            groups_map.entry(g.to_string()).or_default().push(CoverageFeatureView {
                id: n.to_lowercase().replace(' ', "_"),
                name: n.to_string(),
                status: s.to_string(),
                test_ids: vec![],
            });
        }
        let groups: Vec<_> = groups_map.into_iter().map(|(name, feats)| {
            CoverageGroupView {
                id: name.to_lowercase(),
                name,
                role: String::new(),
                covered: feats.iter().filter(|f| f.status != "missing").count(),
                total: feats.len(),
                features: feats,
            }
        }).collect();
        let total_features = groups.iter().map(|g| g.total).sum();
        let total_covered  = groups.iter().map(|g| g.covered).sum();
        CoverageData {
            product: "test".into(), version: "0".into(),
            total_covered, total_features, groups,
            discovered: total_features, scripted: 0,
            discovery_status: DiscoveryStatus::NotRun,
        }
    }

    fn mk_disc(label: &str) -> DiscoveredFeatureView {
        DiscoveredFeatureView {
            route: "/test".into(),
            label: label.into(),
            kind: "button".into(),
            selector: None,
            last_seen: "2026-05-02T00:00:00Z".into(),
        }
    }

    #[test]
    fn gap_when_yaml_doesnt_have_label() {
        let cov = mk_coverage(&[("Buyer", "Login", "confirmed")]);
        let disc = vec![mk_disc("Place Order")];
        let gaps = discovery_gaps(&disc, &cov);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].label, "Place Order");
    }

    #[test]
    fn no_gap_when_yaml_contains_label() {
        let cov = mk_coverage(&[("Buyer", "Place Order Flow", "confirmed")]);
        let disc = vec![mk_disc("Place Order")];
        let gaps = discovery_gaps(&disc, &cov);
        assert!(gaps.is_empty(), "YAML 'Place Order Flow' should match discovered 'Place Order'");
    }

    #[test]
    fn no_gap_when_label_contains_yaml() {
        let cov = mk_coverage(&[("Buyer", "Login", "confirmed")]);
        let disc = vec![mk_disc("Login Page")];
        let gaps = discovery_gaps(&disc, &cov);
        assert!(gaps.is_empty(), "discovered 'Login Page' should match YAML 'Login'");
    }

    #[test]
    fn ui_chrome_filtered_out() {
        let cov = mk_coverage(&[("Buyer", "Real Feature", "confirmed")]);
        let disc = vec![
            mk_disc("Back"),
            mk_disc("Close"),
            mk_disc("Settings"),
            mk_disc("登出"),
            mk_disc("返回"),
        ];
        let gaps = discovery_gaps(&disc, &cov);
        assert!(gaps.is_empty(), "UI chrome should be filtered out, got {gaps:?}");
    }

    #[test]
    fn case_insensitive() {
        let cov = mk_coverage(&[("Buyer", "Place Order", "confirmed")]);
        let disc = vec![mk_disc("PLACE ORDER")];
        let gaps = discovery_gaps(&disc, &cov);
        assert!(gaps.is_empty());
    }

    #[test]
    fn empty_yaml_all_gaps() {
        let cov = mk_coverage(&[]);
        let disc = vec![mk_disc("Foo"), mk_disc("Bar")];
        let gaps = discovery_gaps(&disc, &cov);
        assert_eq!(gaps.len(), 2);
    }
}

fn coverage_color(pct: f32) -> egui::Color32 {
    if pct >= 0.80 { theme::ACCENT }
    else if pct >= 0.50 { theme::YELLOW }
    else { theme::DANGER }
}
