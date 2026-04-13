//! Log view — severity filter + version-cached coloured lines.

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

    ui.horizontal(|ui| {
        ui.label(RichText::new("系統 Log").strong().color(theme::TEXT));
        ui.separator();
        for (label, f, color) in [
            ("全部", Filter::All, theme::BLUE),
            ("⚠ 警告+", Filter::WarnPlus, theme::YELLOW),
            ("✗ 錯誤", Filter::ErrorOnly, theme::RED),
        ] {
            let active = filter == f;
            let btn = egui::Button::new(RichText::new(label).small().color(if active { theme::CRUST } else { theme::SUBTEXT0 }))
                .fill(if active { color } else { Color32::TRANSPARENT }).corner_radius(4.0);
            if ui.add(btn).clicked() { state.filter = Some(f); }
        }
        ui.separator();
        ui.colored_label(theme::OVERLAY0, RichText::new(
            if filter == Filter::All { format!("{shown} 行") } else { format!("{shown} / {total} 行") }
        ).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("🗑 清除").clicked() { svc.log_clear(); }
        });
    });
    ui.separator();

    ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
        if state.cache.is_empty() {
            ui.colored_label(theme::OVERLAY0, "目前沒有符合條件的 Log");
            return;
        }
        for (text, color) in &state.cache {
            ui.colored_label(*color, RichText::new(text.as_str()).monospace().small());
        }
    });
}
