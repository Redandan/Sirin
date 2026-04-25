//! Test Dashboard — recent run history (SQLite) + live active runs (in-memory).
//!
//! Layout:
//!   ┌─────────────────────────────────┐
//!   │  TEST RUNS                      │
//!   │  ⚡ ACTIVE  (if any)            │
//!   │  ● RUN  agora_market_smoke  …   │
//!   │  ─────────────────────────────  │
//!   │  HISTORY (last 30)              │
//!   │  ● PASS agora_webrtc_perm  42s  │
//!   │  ● FAIL agora_pickup_check …    │
//!   └─────────────────────────────────┘

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use crate::ui_service::{AppService, TestRunView};
use super::theme;

// ── State ────────────────────────────────────────────────────────────────────

pub struct TestDashState {
    last_refresh:  std::time::Instant,
    recent:        Vec<TestRunView>,
    active:        Vec<TestRunView>,
}

impl Default for TestDashState {
    fn default() -> Self {
        Self {
            // Force immediate refresh on first frame.
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            recent: Vec::new(),
            active: Vec::new(),
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut TestDashState) {
    // Refresh every 3 seconds (or immediately on first frame).
    if state.last_refresh.elapsed() > std::time::Duration::from_secs(3) {
        state.recent = svc.recent_test_runs(30);
        state.active = svc.active_test_runs();
        state.last_refresh = std::time::Instant::now();
    }

    // Keep the UI live while there are active runs.
    if !state.active.is_empty() {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(3));
    }

    ScrollArea::vertical().id_salt("test_dash").show(ui, |ui| {
        // ── Header ────────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.colored_label(theme::TEXT, RichText::new("TEST RUNS").size(theme::FONT_TITLE).strong());
            ui.add_space(theme::SP_SM);
            let total = state.recent.len();
            let passed = state.recent.iter().filter(|r| r.status == "passed").count();
            ui.colored_label(
                theme::TEXT_DIM,
                RichText::new(format!("{passed}/{total} passed"))
                    .size(theme::FONT_SMALL)
                    .monospace(),
            );
        });
        ui.add_space(theme::SP_SM);
        theme::thin_separator(ui);
        ui.add_space(theme::SP_SM);

        // ── Active runs ───────────────────────────────────────────────────
        if !state.active.is_empty() {
            ui.horizontal(|ui| {
                // Animated pulse dot
                let pulse = (ui.input(|i| i.time).sin() * 0.5 + 0.5) as f32;
                let color = theme::ACCENT.linear_multiply(0.5 + pulse * 0.5);
                ui.colored_label(color, RichText::new("●").size(10.0));
                ui.colored_label(
                    theme::ACCENT,
                    RichText::new(format!("ACTIVE  {} running", state.active.len()))
                        .size(theme::FONT_SMALL)
                        .strong(),
                );
            });
            ui.add_space(theme::SP_XS);

            for run in &state.active {
                show_run_row(ui, run);
            }
            ui.add_space(theme::SP_SM);
            theme::thin_separator(ui);
            ui.add_space(theme::SP_SM);
        }

        // ── Recent history ────────────────────────────────────────────────
        ui.colored_label(
            theme::TEXT_DIM,
            RichText::new("HISTORY  last 30 runs").size(theme::FONT_SMALL).strong(),
        );
        ui.add_space(theme::SP_XS);

        if state.recent.is_empty() {
            ui.add_space(theme::SP_LG);
            ui.centered_and_justified(|ui| {
                ui.colored_label(theme::TEXT_DIM, "No runs recorded yet.");
            });
        } else {
            for run in &state.recent {
                show_run_row(ui, run);
            }
        }
    });
}

// ── Row renderer ──────────────────────────────────────────────────────────────

fn show_run_row(ui: &mut egui::Ui, run: &TestRunView) {
    let (dot_color, badge_text) = match run.status.as_str() {
        "passed"  => (theme::ACCENT,    "PASS"),
        "failed"  => (theme::DANGER,    "FAIL"),
        "timeout" => (theme::YELLOW,    "TIME"),
        "error"   => (theme::DANGER,    "ERR "),
        "running" => (theme::INFO,      "RUN "),
        "queued"  => (theme::TEXT_DIM,  "WAIT"),
        _         => (theme::TEXT_DIM,  "?   "),
    };

    egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(4.0)
        .inner_margin(egui::vec2(theme::SP_MD, theme::SP_XS + 2.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Status dot
                ui.colored_label(dot_color, RichText::new("●").size(9.0));

                // Status badge (monospace fixed-width)
                ui.colored_label(
                    dot_color,
                    RichText::new(badge_text).size(theme::FONT_CAPTION).strong().monospace(),
                );

                // Test id
                ui.colored_label(
                    theme::TEXT,
                    RichText::new(&run.test_id).size(theme::FONT_BODY),
                );

                // Duration — right-aligned
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ms) = run.duration_ms {
                        let s = ms / 1000;
                        let dur = if s >= 60 {
                            format!("{}m{:02}s", s / 60, s % 60)
                        } else {
                            format!("{s}s")
                        };
                        ui.colored_label(
                            theme::TEXT_DIM,
                            RichText::new(dur).size(theme::FONT_CAPTION).monospace(),
                        );
                    }
                });
            });

            // Analysis snippet (single line, truncated)
            if let Some(ref analysis) = run.analysis {
                let max_chars = 100usize;
                let snippet: String = if analysis.chars().count() > max_chars {
                    analysis.chars().take(max_chars).collect::<String>() + "…"
                } else {
                    analysis.clone()
                };
                ui.colored_label(
                    theme::TEXT_DIM,
                    RichText::new(snippet).size(theme::FONT_CAPTION),
                );
            }
        });
    ui.add_space(theme::SP_XS);
}
