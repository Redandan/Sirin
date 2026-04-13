//! Log view — severity filter + version-cached coloured lines.
//! Typography: uses theme::FONT_* for consistent sizing across all pages.

use std::sync::Arc;
use eframe::egui::{self, Color32, RichText, ScrollArea};
use super::theme;
use crate::ui_service::*;

#[derive(Clone, Copy, PartialEq)]
enum Filter { All, WarnPlus, ErrorOnly }

#[derive(Default)]
pub struct LogState {
    filter: Option<Filter>,
    cache: Vec<(String, Color32)>,
    cache_version: usize,
    cache_filter: Option<Filter>,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut LogState) {
    let filter = state.filter.unwrap_or(Filter::All);
    let ver = svc.log_version();
    if ver != state.cache_version || Some(filter) != state.cache_filter {
        let all = svc.log_recent(300);
        state.cache = all.into_iter()
            .filter(|l| match filter {
                Filter::All => true,
                Filter::WarnPlus => matches!(l.level, LogLevel::Error | LogLevel::Warn),
                Filter::ErrorOnly => matches!(l.level, LogLevel::Error),
            })
            .map(|l| (l.text, theme::log_color(l.level)))
            .collect();
        state.cache_version = ver;
        state.cache_filter = Some(filter);
    }

    let total = svc.log_len();
    let shown = state.cache.len();

    // Header
    ui.horizontal(|ui| {
        ui.label(RichText::new("系統 Log").size(theme::FONT_BODY).strong().color(theme::TEXT));
        ui.add_space(theme::SP_MD);

        for (label, f, color) in [
            ("全部", Filter::All, theme::INFO),
            ("⚠ 警告+", Filter::WarnPlus, theme::YELLOW),
            ("✗ 錯誤", Filter::ErrorOnly, theme::DANGER),
        ] {
            let active = filter == f;
            let btn = egui::Button::new(
                RichText::new(label).size(theme::FONT_SMALL).color(if active { theme::BG } else { theme::TEXT_DIM })
            ).fill(if active { color } else { Color32::TRANSPARENT }).corner_radius(4.0);
            if ui.add(btn).clicked() { state.filter = Some(f); }
        }

        ui.add_space(theme::SP_MD);
        let count_text = if filter == Filter::All { format!("{shown} 行") } else { format!("{shown} / {total} 行") };
        ui.colored_label(theme::TEXT_DIM, RichText::new(count_text).size(theme::FONT_CAPTION));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(RichText::new("清除").size(theme::FONT_SMALL).color(theme::TEXT_DIM))
                .fill(Color32::TRANSPARENT).corner_radius(4.0)).clicked() {
                svc.log_clear();
            }
        });
    });
    ui.add_space(theme::SP_SM);

    // Log lines
    ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
        if state.cache.is_empty() {
            ui.add_space(theme::SP_XL);
            ui.colored_label(theme::TEXT_DIM, RichText::new("目前沒有符合條件的 Log").size(theme::FONT_BODY));
            return;
        }
        for (text, color) in &state.cache {
            ui.colored_label(*color, RichText::new(text.as_str()).size(theme::FONT_SMALL).monospace());
        }
    });
}
