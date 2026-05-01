//! System Panel — two tabs:
//!   [Settings]  persona / LLM / diagnostics / connections
//!   [Log]       system log stream

use std::sync::Arc;
use eframe::egui::{self, RichText};
use crate::ui_service::{AppService, AgentSummary};
use super::theme;
use super::settings::SettingsState;
use super::log_view::LogState;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum SysTab { #[default] Settings, Log }

pub struct SystemPanelState {
    pub tab:      SysTab,
    pub settings: SettingsState,
    pub log:      LogState,
}

impl Default for SystemPanelState {
    fn default() -> Self {
        Self {
            tab:      SysTab::default(),
            settings: SettingsState::default(),
            log:      LogState::default(),
        }
    }
}

pub fn show(
    ui:     &mut egui::Ui,
    svc:    &Arc<dyn AppService>,
    agents: &[AgentSummary],
    state:  &mut SystemPanelState,
) {
    ui.horizontal(|ui| {
        tab_btn(ui, "SETTINGS", SysTab::Settings, &mut state.tab);
        ui.add_space(theme::SP_MD);
        tab_btn(ui, "LOG",      SysTab::Log,      &mut state.tab);
    });
    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    match state.tab {
        SysTab::Settings => super::settings::show(ui, svc, agents, &mut state.settings),
        SysTab::Log      => super::log_view::show(ui, svc, &mut state.log),
    }
}

fn tab_btn(ui: &mut egui::Ui, label: &str, tab: SysTab, active: &mut SysTab) {
    let sel  = *active == tab;
    let text = RichText::new(label).size(theme::FONT_SMALL).strong();
    let text = if sel { text.color(theme::ACCENT) } else { text.color(theme::TEXT_DIM) };
    if ui.add(egui::Button::new(text).frame(false)).clicked() { *active = tab; }
    if sel {
        let r = ui.min_rect();
        ui.painter().hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(2.0, theme::ACCENT));
    }
}
