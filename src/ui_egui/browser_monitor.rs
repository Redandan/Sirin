//! Browser + Monitor merged view — two tabs:
//!   [Control]  manual CDP operations  (was browser.rs)
//!   [Monitor]  live screenshot stream  (was monitor view)

use std::sync::Arc;
use eframe::egui::{self, RichText};
use crate::ui_service::AppService;
use super::theme;
use super::browser::BrowserUiState;
use super::monitor::MonitorViewState;

// ── Tab ───────────────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMonitorTab { #[default] Control, Monitor }

#[derive(Default)]
pub struct BrowserMonitorState {
    pub tab:     BrowserMonitorTab,
    pub browser: BrowserUiState,
    pub monitor: MonitorViewState,
}


// ── Entry point ───────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut BrowserMonitorState) {
    // Tab bar.
    ui.horizontal(|ui| {
        tab_btn(ui, "BROWSER CONTROL", BrowserMonitorTab::Control, &mut state.tab);
        ui.add_space(theme::SP_MD);
        tab_btn(ui, "MONITOR", BrowserMonitorTab::Monitor, &mut state.tab);
    });
    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    match state.tab {
        BrowserMonitorTab::Control => super::browser::show(ui, svc, &mut state.browser),
        BrowserMonitorTab::Monitor => super::monitor::show(ui, &mut state.monitor),
    }
}

fn tab_btn(ui: &mut egui::Ui, label: &str, tab: BrowserMonitorTab, active: &mut BrowserMonitorTab) {
    let selected = *active == tab;
    let text = RichText::new(label).size(theme::FONT_SMALL).strong();
    let text = if selected { text.color(theme::ACCENT) } else { text.color(theme::TEXT_DIM) };
    if ui.add(egui::Button::new(text).frame(false)).clicked() {
        *active = tab;
    }
    if selected {
        let r = ui.min_rect();
        ui.painter().hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(2.0, theme::ACCENT));
    }
}
