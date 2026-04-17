//! Real-time action event feed.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use crate::monitor::events::ServerEvent;
use crate::monitor::state::MonitorState;
use crate::ui_egui::theme;

pub fn show(
    ui: &mut egui::Ui,
    monitor_state: &Option<Arc<MonitorState>>,
    auto_scroll: &mut bool,
) {
    theme::card(ui, |ui| {
        // Header
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("ACTION FEED")
                    .size(theme::FONT_SMALL)
                    .color(theme::TEXT_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(
                    auto_scroll,
                    RichText::new("Auto-scroll").size(theme::FONT_SMALL),
                );
            });
        });
        ui.add_space(theme::SP_SM);

        let events = monitor_state
            .as_ref()
            .map(|ms| ms.events_snapshot())
            .unwrap_or_default();

        ScrollArea::vertical()
            .id_salt("monitor_feed")
            .auto_shrink([false, false])
            .stick_to_bottom(*auto_scroll)
            .show(ui, |ui| {
                if events.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(
                            theme::TEXT_DIM,
                            "No events yet \u{2014} start an MCP session",
                        );
                    });
                    return;
                }

                for event in &events {
                    render_event(ui, event);
                }
            });
    });
}

fn render_event(ui: &mut egui::Ui, event: &ServerEvent) {
    match event {
        ServerEvent::ActionStart {
            id: _,
            client,
            action,
            args,
            ts,
        } => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(ts.format("%H:%M:%S%.3f").to_string())
                        .size(theme::FONT_CAPTION)
                        .color(theme::TEXT_DIM)
                        .monospace(),
                );
                ui.add_space(theme::SP_SM);
                ui.label(
                    RichText::new(client.as_str())
                        .size(theme::FONT_SMALL)
                        .color(theme::INFO),
                );
                ui.add_space(theme::SP_SM);
                ui.label(
                    RichText::new(format!("\u{25b6} {action}"))
                        .size(theme::FONT_SMALL)
                        .color(theme::TEXT),
                );
                // Show key args inline
                if let Some(target) = args.get("target").and_then(|v| v.as_str()) {
                    ui.label(
                        RichText::new(format!("  {target}"))
                            .size(theme::FONT_SMALL)
                            .color(theme::TEXT_DIM),
                    );
                }
            });
        }
        ServerEvent::ActionDone {
            id: _,
            result: _,
            duration_ms,
            ts,
        } => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(ts.format("%H:%M:%S%.3f").to_string())
                        .size(theme::FONT_CAPTION)
                        .color(theme::TEXT_DIM)
                        .monospace(),
                );
                ui.add_space(theme::SP_SM);
                ui.colored_label(
                    theme::ACCENT,
                    RichText::new("\u{2713}").size(theme::FONT_SMALL),
                );
                ui.label(
                    RichText::new(format!("{duration_ms}ms"))
                        .size(theme::FONT_SMALL)
                        .color(theme::TEXT_DIM),
                );
            });
        }
        ServerEvent::ActionError { id: _, error, ts } => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(ts.format("%H:%M:%S%.3f").to_string())
                        .size(theme::FONT_CAPTION)
                        .color(theme::TEXT_DIM)
                        .monospace(),
                );
                ui.add_space(theme::SP_SM);
                ui.colored_label(
                    theme::DANGER,
                    RichText::new("\u{2717}").size(theme::FONT_SMALL),
                );
                ui.label(
                    RichText::new(error.as_str())
                        .size(theme::FONT_SMALL)
                        .color(theme::DANGER),
                );
            });
        }
        ServerEvent::AuthzAsk {
            request_id: _,
            ts,
            client,
            action,
            url,
            ..
        } => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(ts.format("%H:%M:%S%.3f").to_string())
                        .size(theme::FONT_CAPTION)
                        .color(theme::TEXT_DIM)
                        .monospace(),
                );
                ui.add_space(theme::SP_SM);
                ui.colored_label(
                    theme::YELLOW,
                    RichText::new("\u{26a0} AUTHZ ASK")
                        .size(theme::FONT_SMALL)
                        .strong(),
                );
                ui.add_space(theme::SP_SM);
                ui.label(
                    RichText::new(format!("{client}  {action}  ({url})"))
                        .size(theme::FONT_SMALL),
                );
            });
        }
        ServerEvent::UrlChange { url, ts } => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(ts.format("%H:%M:%S%.3f").to_string())
                        .size(theme::FONT_CAPTION)
                        .color(theme::TEXT_DIM)
                        .monospace(),
                );
                ui.add_space(theme::SP_SM);
                ui.label(
                    RichText::new("\u{2197}")
                        .size(theme::FONT_SMALL)
                        .color(theme::TEXT_DIM),
                );
                ui.label(
                    RichText::new(url.as_str())
                        .size(theme::FONT_SMALL)
                        .color(theme::TEXT_DIM),
                );
            });
        }
        // Hello, Goodbye, State, Screenshot, Console, Network, AuthzResolved
        _ => {
            ui.horizontal(|ui| {
                let ts = event.ts();
                ui.label(
                    RichText::new(ts.format("%H:%M:%S%.3f").to_string())
                        .size(theme::FONT_CAPTION)
                        .color(theme::TEXT_DIM)
                        .monospace(),
                );
                ui.add_space(theme::SP_SM);
                ui.label(
                    RichText::new(event_label(event))
                        .size(theme::FONT_SMALL)
                        .color(theme::TEXT_DIM),
                );
            });
        }
    }
    ui.add_space(1.0);
}

fn event_label(event: &ServerEvent) -> &'static str {
    match event {
        ServerEvent::Hello { .. } => "\u{25cf} session start",
        ServerEvent::Goodbye { .. } => "\u{25cb} session end",
        ServerEvent::Screenshot { .. } => "screenshot",
        ServerEvent::State { .. } => "state change",
        ServerEvent::Console { .. } => "console",
        ServerEvent::Network { .. } => "network",
        ServerEvent::AuthzResolved { .. } => "authz resolved",
        // Variants with dedicated arms above should never reach here,
        // but the catch-all keeps the match exhaustive.
        _ => "event",
    }
}
