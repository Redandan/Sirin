//! Authz ask panel — shows pending authz requests with interactive Allow/Deny.

use eframe::egui::{self, RichText};

use crate::monitor::state::AuthzDecisionResult;
use crate::ui_egui::theme;

/// Render pending authz asks as an inline panel (shown at top of Monitor when asks exist).
/// Returns true if at least one ask is pending (caller may want to highlight the view).
pub fn show(ui: &mut egui::Ui) -> bool {
    let Some(ms) = crate::monitor::state() else {
        return false;
    };
    let pending = ms.pending_ask_ids();
    if pending.is_empty() {
        return false;
    }

    // Highlight panel — yellow tint border
    let frame = egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(0xFF, 0xD9, 0x3D, 20))
        .inner_margin(theme::SP_MD)
        .stroke(egui::Stroke::new(1.0, theme::YELLOW))
        .corner_radius(4.0);

    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(
                theme::YELLOW,
                RichText::new("! AUTHZ ASK")
                    .size(theme::FONT_SMALL)
                    .strong(),
            );
            ui.add_space(theme::SP_SM);
            ui.label(
                RichText::new(format!(
                    "{} action(s) waiting for approval",
                    pending.len()
                ))
                .size(theme::FONT_SMALL)
                .color(theme::TEXT_DIM),
            );
        });

        ui.add_space(theme::SP_SM);

        // Show each pending ask — look up event details from the live feed
        let events = ms.events_snapshot();
        for req_id in &pending {
            let ask_event = events.iter().find(|e| {
                matches!(e, crate::monitor::events::ServerEvent::AuthzAsk { request_id, .. }
                    if request_id == req_id)
            });

            ui.horizontal(|ui| {
                // Show action info from the event if available
                if let Some(crate::monitor::events::ServerEvent::AuthzAsk {
                    client,
                    action,
                    url,
                    ..
                }) = ask_event
                {
                    ui.label(
                        RichText::new(format!("{client}  {action}"))
                            .size(theme::FONT_SMALL)
                            .color(theme::TEXT),
                    );
                    ui.add_space(theme::SP_SM);
                    ui.label(
                        RichText::new(url)
                            .size(theme::FONT_SMALL)
                            .color(theme::TEXT_DIM),
                    );
                } else {
                    ui.label(
                        RichText::new(req_id)
                            .size(theme::FONT_SMALL)
                            .color(theme::TEXT_DIM),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Deny button
                    let deny_btn = ui.add(
                        egui::Button::new(
                            RichText::new("x Deny").color(theme::DANGER),
                        )
                        .fill(theme::CARD)
                        .corner_radius(4.0),
                    );
                    if deny_btn.clicked() {
                        ms.resolve_authz_ask(req_id, AuthzDecisionResult::Deny);
                    }

                    ui.add_space(theme::SP_SM);

                    // Allow button
                    let allow_btn = ui.add(
                        egui::Button::new(
                            RichText::new("v Allow").color(theme::BG),
                        )
                        .fill(theme::ACCENT)
                        .corner_radius(4.0),
                    );
                    if allow_btn.clicked() {
                        ms.resolve_authz_ask(req_id, AuthzDecisionResult::Allow);
                    }
                });
            });
        }
    });

    true
}
