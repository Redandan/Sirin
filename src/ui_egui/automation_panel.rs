//! Automation Panel — two tabs:
//!   [Dev Squad]  PM/Engineer/Tester + GitHub bridge + task queue
//!   [MCP]        tool browser + executor

use std::sync::Arc;
use eframe::egui::{self, RichText};
use crate::ui_service::AppService;
use super::theme;
use super::team_panel::TeamPanelState;
use super::mcp_playground::McpPlaygroundState;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum AutoTab { #[default] Squad, Mcp }

pub struct AutomationPanelState {
    pub tab:  AutoTab,
    pub team: TeamPanelState,
    pub mcp:  McpPlaygroundState,
}

impl Default for AutomationPanelState {
    fn default() -> Self {
        Self {
            tab:  AutoTab::default(),
            team: TeamPanelState::default(),
            mcp:  McpPlaygroundState::default(),
        }
    }
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut AutomationPanelState) {
    ui.horizontal(|ui| {
        tab_btn(ui, "DEV SQUAD",      AutoTab::Squad, &mut state.tab);
        ui.add_space(theme::SP_MD);
        tab_btn(ui, "MCP PLAYGROUND", AutoTab::Mcp,   &mut state.tab);
    });
    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    match state.tab {
        AutoTab::Squad => super::team_panel::show(ui, svc, &mut state.team),
        AutoTab::Mcp   => super::mcp_playground::show(ui, svc, &mut state.mcp),
    }
}

fn tab_btn(ui: &mut egui::Ui, label: &str, tab: AutoTab, active: &mut AutoTab) {
    let sel  = *active == tab;
    let text = RichText::new(label).size(theme::FONT_SMALL).strong();
    let text = if sel { text.color(theme::ACCENT) } else { text.color(theme::TEXT_DIM) };
    if ui.add(egui::Button::new(text).frame(false)).clicked() { *active = tab; }
    if sel {
        let r = ui.min_rect();
        ui.painter().hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(2.0, theme::ACCENT));
    }
}
