//! Log view — system log with severity filter and version-based caching.
//!
//! AI reads this: header with filter buttons (All/Warn/Error) + line count,
//! scrollable log area with colour-coded lines by module.

use std::sync::Arc;

use eframe::egui::{self, Color32, RichText, ScrollArea};

use crate::ui_service::*;

#[derive(Clone, Copy, PartialEq)]
enum Filter { All, WarnPlus, ErrorOnly }

#[derive(Default)]
pub struct LogState {
    filter: Option<Filter>, // None → use All
    cache: Vec<(String, Color32)>,
    cache_version: usize,
    cache_filter: Option<Filter>,
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut LogState) {
    let filter = state.filter.unwrap_or(Filter::All);

    // Rebuild cache only when version or filter changes
    let ver = svc.log_version();
    if ver != state.cache_version || Some(filter) != state.cache_filter {
        let all = svc.log_recent(300);
        state.cache = all
            .into_iter()
            .filter(|l| match filter {
                Filter::All => true,
                Filter::WarnPlus => matches!(l.level, LogLevel::Error | LogLevel::Warn),
                Filter::ErrorOnly => matches!(l.level, LogLevel::Error),
            })
            .map(|l| (l.text, level_color(l.level)))
            .collect();
        state.cache_version = ver;
        state.cache_filter = Some(filter);
    }

    let total = svc.log_len();
    let shown = state.cache.len();

    // Header
    ui.horizontal(|ui| {
        ui.label(RichText::new("系統 Log").strong());
        ui.separator();

        for (label, f, color) in [
            ("全部", Filter::All, Color32::from_rgb(80, 130, 200)),
            ("⚠ 警告+", Filter::WarnPlus, Color32::from_rgb(200, 160, 40)),
            ("✗ 錯誤", Filter::ErrorOnly, Color32::from_rgb(200, 70, 70)),
        ] {
            let active = filter == f;
            let btn = egui::Button::new(RichText::new(label).small())
                .fill(if active { color } else { Color32::TRANSPARENT });
            if ui.add(btn).clicked() {
                state.filter = Some(f);
            }
        }

        ui.separator();
        let count_text = if filter == Filter::All {
            format!("{shown} 行")
        } else {
            format!("{shown} / {total} 行")
        };
        ui.colored_label(Color32::DARK_GRAY, RichText::new(count_text).small());

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("🗑 清除").clicked() {
                svc.log_clear();
            }
        });
    });
    ui.separator();

    // Log lines
    ScrollArea::vertical()
        .id_salt("log_view")
        .stick_to_bottom(true)
        .auto_shrink(false)
        .show(ui, |ui| {
            if state.cache.is_empty() {
                ui.colored_label(Color32::DARK_GRAY, "目前沒有符合條件的 Log");
                return;
            }
            for (text, color) in &state.cache {
                ui.colored_label(*color, RichText::new(text.as_str()).monospace().small());
            }
        });
}

fn level_color(level: LogLevel) -> Color32 {
    match level {
        LogLevel::Error => Color32::from_rgb(220, 100, 100),
        LogLevel::Warn => Color32::from_rgb(220, 180, 80),
        LogLevel::Telegram => Color32::from_rgb(100, 180, 255),
        LogLevel::Research => Color32::from_rgb(150, 220, 150),
        LogLevel::Followup => Color32::from_rgb(220, 180, 100),
        LogLevel::Coding => Color32::from_rgb(180, 150, 255),
        LogLevel::Teams => Color32::from_rgb(100, 200, 220),
        LogLevel::Info | LogLevel::Normal => Color32::GRAY,
    }
}
