//! Test Dashboard — recent run history (SQLite) + live active runs (in-memory)
//! + ad-hoc test launcher + status/text filter + inline pass-rate sparkline.
//!
//! Layout:
//!   ┌─────────────────────────────────────────────┐
//!   │  TEST RUNS         18/30 passed  ▒▒▒█▒▒▒▒▒  │ ← header + sparkline
//!   │  ────────────────────────────────────────── │
//!   │  RUN [agora_market_smoke ▾] [▶ Run]         │ ← launcher
//!   │  Filter: [All|Pass|Fail] [search…]          │ ← filter
//!   │  ────────────────────────────────────────── │
//!   │  ⚡ ACTIVE  (if any)                        │
//!   │  ● RUN  agora_market_smoke  …               │
//!   │  ────────────────────────────────────────── │
//!   │  HISTORY (last 30, filtered)                │
//!   │  ● PASS agora_webrtc_perm  42s              │
//!   │  ● FAIL agora_pickup_check …                │
//!   └─────────────────────────────────────────────┘

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use crate::ui_service::{AppService, TestRunView};
use super::theme;

// ── State ────────────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    #[default]
    All,
    Passed,
    Failed,
}

pub struct TestDashState {
    last_refresh:    std::time::Instant,
    recent:          Vec<TestRunView>,
    active:          Vec<TestRunView>,
    /// Cached YAML test_ids for the launcher dropdown — refreshed on demand.
    test_ids:        Vec<String>,
    test_ids_loaded: bool,
    /// Currently selected test_id in the launcher dropdown.
    selected_test:   String,
    /// Toast message shown after launch attempt — "✓ launched run_… " or
    /// "✗ <error>".  Auto-clears after ~5 seconds.
    last_launch_msg: Option<(String, std::time::Instant)>,
    /// Filter UI state.
    status_filter:   StatusFilter,
    text_filter:     String,
}

impl Default for TestDashState {
    fn default() -> Self {
        Self {
            // Force immediate refresh on first frame.
            last_refresh: std::time::Instant::now() - std::time::Duration::from_secs(60),
            recent: Vec::new(),
            active: Vec::new(),
            test_ids: Vec::new(),
            test_ids_loaded: false,
            selected_test: String::new(),
            last_launch_msg: None,
            status_filter: StatusFilter::default(),
            text_filter: String::new(),
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

    // Lazy-load test_ids once — file scan is cheap but no need every frame.
    if !state.test_ids_loaded {
        state.test_ids = svc.list_test_ids();
        state.test_ids_loaded = true;
        if state.selected_test.is_empty() {
            if let Some(first) = state.test_ids.first() {
                state.selected_test = first.clone();
            }
        }
    }

    // Keep the UI live while there are active runs OR a launch toast is showing.
    if !state.active.is_empty() || state.last_launch_msg.is_some() {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    }

    // Auto-clear stale launch toast.
    if let Some((_, t)) = &state.last_launch_msg {
        if t.elapsed() > std::time::Duration::from_secs(5) {
            state.last_launch_msg = None;
        }
    }

    ScrollArea::vertical().id_salt("test_dash").show(ui, |ui| {
        // ── Header + sparkline ────────────────────────────────────────────
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
            ui.add_space(theme::SP_SM);
            draw_sparkline(ui, &state.recent);
        });
        ui.add_space(theme::SP_SM);
        theme::thin_separator(ui);
        ui.add_space(theme::SP_SM);

        // ── Launcher row ──────────────────────────────────────────────────
        show_launcher(ui, svc, state);
        ui.add_space(theme::SP_XS);

        // ── Filter row ────────────────────────────────────────────────────
        show_filter_row(ui, state);
        ui.add_space(theme::SP_SM);
        theme::thin_separator(ui);
        ui.add_space(theme::SP_SM);

        // ── Active runs (never filtered — always visible) ─────────────────
        if !state.active.is_empty() {
            ui.horizontal(|ui| {
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

        // ── Recent history (filtered) ─────────────────────────────────────
        let filtered: Vec<&TestRunView> = state
            .recent
            .iter()
            .filter(|r| status_filter_matches(state.status_filter, &r.status))
            .filter(|r| text_filter_matches(&state.text_filter, &r.test_id))
            .collect();

        let header_label = if state.status_filter == StatusFilter::All && state.text_filter.is_empty() {
            format!("HISTORY  last {} runs", state.recent.len())
        } else {
            format!("HISTORY  {}/{} match filter", filtered.len(), state.recent.len())
        };
        ui.colored_label(
            theme::TEXT_DIM,
            RichText::new(header_label).size(theme::FONT_SMALL).strong(),
        );
        ui.add_space(theme::SP_XS);

        if filtered.is_empty() {
            ui.add_space(theme::SP_LG);
            ui.centered_and_justified(|ui| {
                let msg = if state.recent.is_empty() {
                    "No runs recorded yet."
                } else {
                    "No runs match the current filter."
                };
                ui.colored_label(theme::TEXT_DIM, msg);
            });
        } else {
            for run in filtered {
                show_run_row(ui, run);
            }
        }
    });
}

// ── Launcher row (dropdown + Run button + last-launch toast) ─────────────────

fn show_launcher(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut TestDashState) {
    ui.horizontal(|ui| {
        ui.colored_label(
            theme::TEXT_DIM,
            RichText::new("RUN").size(theme::FONT_SMALL).strong().monospace(),
        );

        // Dropdown of YAML test_ids.  ComboBox::width keeps it from stretching.
        let combo_label = if state.selected_test.is_empty() {
            "(no tests configured)".to_string()
        } else {
            state.selected_test.clone()
        };
        egui::ComboBox::from_id_salt("test_dash_launcher")
            .width(280.0)
            .selected_text(combo_label)
            .show_ui(ui, |ui| {
                for id in &state.test_ids {
                    ui.selectable_value(&mut state.selected_test, id.clone(), id);
                }
            });

        // Disable Run while no test is selected or there's already an active
        // run for the same test_id (avoid duplicate spawns).
        let already_active = state
            .active
            .iter()
            .any(|r| r.test_id == state.selected_test);
        let enabled = !state.selected_test.is_empty() && !already_active;

        let btn = egui::Button::new(
            RichText::new("▶ Run").size(theme::FONT_SMALL).strong(),
        );
        if ui.add_enabled(enabled, btn).clicked() {
            match svc.launch_test_run(&state.selected_test) {
                Ok(run_id) => {
                    state.last_launch_msg = Some((
                        format!("✓ launched {run_id}"),
                        std::time::Instant::now(),
                    ));
                    // Force quick refresh so the active-runs section shows it.
                    state.last_refresh = std::time::Instant::now()
                        - std::time::Duration::from_secs(60);
                }
                Err(e) => {
                    state.last_launch_msg = Some((
                        format!("✗ {e}"),
                        std::time::Instant::now(),
                    ));
                }
            }
        }

        if already_active {
            ui.colored_label(
                theme::YELLOW,
                RichText::new("(already running)").size(theme::FONT_CAPTION),
            );
        }

        if let Some((msg, _)) = &state.last_launch_msg {
            let col = if msg.starts_with("✓") { theme::ACCENT } else { theme::DANGER };
            ui.colored_label(col, RichText::new(msg).size(theme::FONT_CAPTION).monospace());
        }
    });
}

// ── Filter row (3 status tabs + text edit) ───────────────────────────────────

fn show_filter_row(ui: &mut egui::Ui, state: &mut TestDashState) {
    ui.horizontal(|ui| {
        ui.colored_label(
            theme::TEXT_DIM,
            RichText::new("FILTER").size(theme::FONT_SMALL).strong().monospace(),
        );

        ui.selectable_value(&mut state.status_filter, StatusFilter::All, "All");
        ui.selectable_value(&mut state.status_filter, StatusFilter::Passed, "Passed");
        ui.selectable_value(&mut state.status_filter, StatusFilter::Failed, "Failed");

        ui.add_space(theme::SP_SM);

        ui.add(
            egui::TextEdit::singleline(&mut state.text_filter)
                .hint_text("filter by test_id…")
                .desired_width(220.0)
                .font(egui::TextStyle::Monospace),
        );

        // Quick "clear" affordance when filters are active.
        if state.status_filter != StatusFilter::All || !state.text_filter.is_empty() {
            if ui
                .add(egui::Button::new(RichText::new("✕").size(theme::FONT_CAPTION)))
                .on_hover_text("clear filter")
                .clicked()
            {
                state.status_filter = StatusFilter::All;
                state.text_filter.clear();
            }
        }
    });
}

// ── Sparkline (inline pass/fail bar over recent runs) ────────────────────────

/// Renders a 9-cell colour bar — one cell per run, oldest left.  Green = passed,
/// red = failed, grey = anything else (running/queued/error/timeout).  Compact
/// alternative to a full chart; fits in the header row.
fn draw_sparkline(ui: &mut egui::Ui, runs: &[TestRunView]) {
    const CELL_W: f32 = 6.0;
    const CELL_H: f32 = 12.0;
    const GAP:    f32 = 1.0;
    const N:      usize = 12; // last 12 runs

    if runs.is_empty() {
        return;
    }

    // SQLite returns newest-first — reverse so oldest is leftmost in the bar.
    let mut last: Vec<&TestRunView> = runs.iter().rev().take(N).collect();
    last.reverse();

    let total_w = (CELL_W + GAP) * last.len() as f32;
    let (rect, _resp) = ui.allocate_exact_size(
        egui::vec2(total_w, CELL_H),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    let mut x = rect.left();
    for r in last {
        let col = match r.status.as_str() {
            "passed"           => theme::ACCENT,
            "failed" | "error" => theme::DANGER,
            "timeout"          => theme::YELLOW,
            _                  => theme::TEXT_DIM,
        };
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(x, rect.top()),
                egui::vec2(CELL_W, CELL_H),
            ),
            1.0,
            col,
        );
        x += CELL_W + GAP;
    }
}

// ── Filter helpers ───────────────────────────────────────────────────────────

fn status_filter_matches(filter: StatusFilter, status: &str) -> bool {
    match filter {
        StatusFilter::All    => true,
        StatusFilter::Passed => status == "passed",
        StatusFilter::Failed => matches!(status, "failed" | "error" | "timeout"),
    }
}

fn text_filter_matches(needle: &str, test_id: &str) -> bool {
    if needle.is_empty() { return true; }
    test_id.to_lowercase().contains(&needle.to_lowercase())
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
                ui.colored_label(dot_color, RichText::new("●").size(9.0));
                ui.colored_label(
                    dot_color,
                    RichText::new(badge_text).size(theme::FONT_CAPTION).strong().monospace(),
                );
                ui.colored_label(
                    theme::TEXT,
                    RichText::new(&run.test_id).size(theme::FONT_BODY),
                );
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_filter_all_matches_everything() {
        for s in ["passed", "failed", "running", "queued", "error", "timeout", ""] {
            assert!(status_filter_matches(StatusFilter::All, s), "status={s}");
        }
    }

    #[test]
    fn status_filter_passed_only_passed() {
        assert!(status_filter_matches(StatusFilter::Passed, "passed"));
        for s in ["failed", "running", "queued", "error", "timeout", ""] {
            assert!(!status_filter_matches(StatusFilter::Passed, s), "status={s}");
        }
    }

    /// Failed bucket intentionally includes `error` and `timeout` — users
    /// debugging "what broke" want all three lumped together.
    #[test]
    fn status_filter_failed_buckets_error_and_timeout() {
        for s in ["failed", "error", "timeout"] {
            assert!(status_filter_matches(StatusFilter::Failed, s), "status={s}");
        }
        for s in ["passed", "running", "queued", ""] {
            assert!(!status_filter_matches(StatusFilter::Failed, s), "status={s}");
        }
    }

    #[test]
    fn text_filter_empty_matches_anything() {
        assert!(text_filter_matches("", "agora_login"));
        assert!(text_filter_matches("", ""));
    }

    #[test]
    fn text_filter_substring_case_insensitive() {
        assert!(text_filter_matches("login", "agora_login_vision"));
        assert!(text_filter_matches("LOGIN", "agora_login_vision"));
        assert!(text_filter_matches("agora", "agora_login_vision"));
        assert!(!text_filter_matches("xxxxx", "agora_login_vision"));
    }
}
