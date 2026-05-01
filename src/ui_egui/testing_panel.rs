//! Testing Panel — unified panel with three tabs:
//!   [Runs]     test execution history + launcher
//!   [Coverage] feature map progress
//!   [Browser]  CDP control + monitor

use std::sync::Arc;
use eframe::egui::{self, RichText};
use crate::ui_service::AppService;
use super::theme;
use super::test_dashboard::TestDashState;
use super::coverage_panel::CoveragePanelState;
use super::browser_monitor::BrowserMonitorState;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum TestingTab { #[default] Runs, Coverage, Browser }

pub struct TestingPanelState {
    pub tab:      TestingTab,
    pub runs:     TestDashState,
    pub coverage: CoveragePanelState,
    pub browser:  BrowserMonitorState,
}

impl Default for TestingPanelState {
    fn default() -> Self {
        Self {
            tab:      TestingTab::default(),
            runs:     TestDashState::default(),
            coverage: CoveragePanelState::default(),
            browser:  BrowserMonitorState::default(),
        }
    }
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut TestingPanelState) {
    ui.horizontal(|ui| {
        tab_btn(ui, "TEST RUNS", TestingTab::Runs,     &mut state.tab);
        ui.add_space(theme::SP_MD);
        tab_btn(ui, "COVERAGE",  TestingTab::Coverage, &mut state.tab);
        ui.add_space(theme::SP_MD);
        tab_btn(ui, "BROWSER",   TestingTab::Browser,  &mut state.tab);
    });
    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    match state.tab {
        TestingTab::Runs     => super::test_dashboard::show(ui, svc, &mut state.runs),
        TestingTab::Coverage => super::coverage_panel::show(ui, svc, &mut state.coverage),
        TestingTab::Browser  => super::browser_monitor::show(ui, svc, &mut state.browser),
    }
}

fn tab_btn(ui: &mut egui::Ui, label: &str, tab: TestingTab, active: &mut TestingTab) {
    let sel  = *active == tab;
    let text = RichText::new(label).size(theme::FONT_SMALL).strong();
    let text = if sel { text.color(theme::ACCENT) } else { text.color(theme::TEXT_DIM) };
    if ui.add(egui::Button::new(text).frame(false)).clicked() { *active = tab; }
    if sel {
        let r = ui.min_rect();
        ui.painter().hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(2.0, theme::ACCENT));
    }
}
