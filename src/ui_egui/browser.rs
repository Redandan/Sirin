//! Browser control panel — persistent Chrome session with screenshot preview.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea, TextureHandle};
use super::theme;
use crate::ui_service::*;

pub struct BrowserUiState {
    url_input: String,
    selector_input: String,
    type_input: String,
    js_input: String,
    last_result: String,
    headless: bool,
    screenshot_tex: Option<TextureHandle>,
    screenshot_size: [usize; 2],
}

impl Default for BrowserUiState {
    fn default() -> Self {
        Self {
            url_input: String::new(),
            selector_input: String::new(),
            type_input: String::new(),
            js_input: String::new(),
            last_result: String::new(),
            headless: true,
            screenshot_tex: None,
            screenshot_size: [0, 0],
        }
    }
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut BrowserUiState) {
    let is_open = svc.browser_is_open();

    // ── Top bar: URL + controls ──────────────────────────────────────────
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            let status_color = if is_open { theme::ACCENT } else { theme::TEXT_DIM };
            let status_text = if is_open { "LIVE" } else { "OFF" };
            ui.colored_label(status_color, RichText::new(status_text).size(theme::FONT_SMALL).strong());

            ui.add_space(theme::SP_SM);

            let resp = ui.add_sized(
                [ui.available_width() - 180.0, 24.0],
                egui::TextEdit::singleline(&mut state.url_input)
                    .hint_text("https://...")
                    .font(egui::TextStyle::Monospace.resolve(ui.style())),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

            if ui.add(egui::Button::new(RichText::new("Go").color(theme::BG))
                .fill(theme::ACCENT).corner_radius(4.0)).clicked() || enter
            {
                if !state.url_input.trim().is_empty() {
                    if !is_open {
                        svc.browser_open(&state.url_input, state.headless);
                    } else {
                        let _ = svc.browser_navigate(&state.url_input);
                    }
                    refresh_screenshot(svc, state, ui.ctx());
                }
            }

            if ui.add(egui::Button::new("Screenshot")
                .fill(theme::CARD).corner_radius(4.0)).clicked()
            {
                refresh_screenshot(svc, state, ui.ctx());
            }

            if is_open {
                if ui.add(egui::Button::new(RichText::new("Close").color(theme::BG))
                    .fill(theme::DANGER).corner_radius(4.0)).clicked()
                {
                    svc.browser_close();
                    state.screenshot_tex = None;
                    state.last_result.clear();
                }
            }
        });

        ui.horizontal(|ui| {
            ui.checkbox(&mut state.headless, RichText::new("Headless").color(theme::TEXT_DIM).size(theme::FONT_SMALL));

            if let Some(url) = svc.browser_url() {
                ui.colored_label(theme::TEXT_DIM, RichText::new(url).size(theme::FONT_CAPTION));
            }
        });
    });

    ui.add_space(theme::SP_SM);

    // ── Screenshot preview ───────────────────────────────────────────────
    let avail = ui.available_height();
    let preview_h = (avail * 0.55).max(200.0);

    theme::card(ui, |ui| {
        if let Some(tex) = &state.screenshot_tex {
            let [w, h] = state.screenshot_size;
            let aspect = w as f32 / h.max(1) as f32;
            let disp_w = ui.available_width().min(preview_h * aspect);
            let disp_h = disp_w / aspect;
            ScrollArea::both().id_salt("browser_preview").max_height(preview_h).show(ui, |ui| {
                ui.image(egui::load::SizedTexture::new(tex.id(), egui::vec2(disp_w, disp_h)));
            });
        } else {
            ui.vertical_centered(|ui| {
                ui.add_space(preview_h * 0.35);
                ui.colored_label(theme::TEXT_DIM, "No screenshot — navigate to a page first");
            });
            ui.add_space(preview_h * 0.35);
        }
    });

    ui.add_space(theme::SP_SM);

    // ── Quick actions ────────────────────────────────────────────────────
    theme::card(ui, |ui| {
        ui.label(RichText::new("ACTIONS").size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
        ui.add_space(theme::SP_SM);

        // Click / Read row
        ui.horizontal(|ui| {
            ui.label(RichText::new("Selector").size(theme::FONT_SMALL).color(theme::TEXT_DIM));
            ui.add_sized(
                [ui.available_width() - 160.0, 22.0],
                egui::TextEdit::singleline(&mut state.selector_input)
                    .hint_text("#id / .class / tag")
                    .font(egui::TextStyle::Monospace.resolve(ui.style())),
            );

            if ui.add_enabled(is_open, egui::Button::new("Click")
                .fill(theme::CARD).corner_radius(4.0)).clicked()
            {
                match svc.browser_click(&state.selector_input) {
                    Ok(()) => {
                        state.last_result = format!("Clicked: {}", state.selector_input);
                        refresh_screenshot(svc, state, ui.ctx());
                    }
                    Err(e) => state.last_result = format!("ERROR: {e}"),
                }
            }

            if ui.add_enabled(is_open, egui::Button::new("Read")
                .fill(theme::CARD).corner_radius(4.0)).clicked()
            {
                match svc.browser_read(&state.selector_input) {
                    Ok(text) => state.last_result = text,
                    Err(e) => state.last_result = format!("ERROR: {e}"),
                }
            }
        });

        // Type row
        ui.horizontal(|ui| {
            ui.label(RichText::new("Type   ").size(theme::FONT_SMALL).color(theme::TEXT_DIM));
            ui.add_sized(
                [ui.available_width() - 80.0, 22.0],
                egui::TextEdit::singleline(&mut state.type_input)
                    .hint_text("text to type...")
                    .font(egui::TextStyle::Monospace.resolve(ui.style())),
            );

            if ui.add_enabled(is_open && !state.selector_input.is_empty(),
                egui::Button::new("Send").fill(theme::CARD).corner_radius(4.0)).clicked()
            {
                match svc.browser_type(&state.selector_input, &state.type_input) {
                    Ok(()) => {
                        state.last_result = format!("Typed {} chars", state.type_input.len());
                        state.type_input.clear();
                    }
                    Err(e) => state.last_result = format!("ERROR: {e}"),
                }
            }
        });

        // JS eval row
        ui.horizontal(|ui| {
            ui.label(RichText::new("JS     ").size(theme::FONT_SMALL).color(theme::TEXT_DIM));
            ui.add_sized(
                [ui.available_width() - 80.0, 22.0],
                egui::TextEdit::singleline(&mut state.js_input)
                    .hint_text("document.title")
                    .font(egui::TextStyle::Monospace.resolve(ui.style())),
            );

            if ui.add_enabled(is_open, egui::Button::new("Eval")
                .fill(theme::CARD).corner_radius(4.0)).clicked()
            {
                match svc.browser_eval(&state.js_input) {
                    Ok(result) => state.last_result = result,
                    Err(e) => state.last_result = format!("ERROR: {e}"),
                }
            }
        });

        // Result display
        if !state.last_result.is_empty() {
            ui.add_space(theme::SP_SM);
            ui.separator();
            ui.add_space(theme::SP_SM);
            ScrollArea::vertical().id_salt("browser_result").max_height(80.0).show(ui, |ui| {
                ui.label(RichText::new(&state.last_result)
                    .font(egui::TextStyle::Monospace.resolve(ui.style()))
                    .color(theme::TEXT));
            });
        }
    });
}

fn refresh_screenshot(svc: &Arc<dyn AppService>, state: &mut BrowserUiState, ctx: &egui::Context) {
    if let Some(png) = svc.browser_screenshot() {
        if let Ok(image) = image::load_from_memory(&png) {
            let rgba = image.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            state.screenshot_tex = Some(ctx.load_texture(
                "browser_screenshot",
                color_image,
                egui::TextureOptions::LINEAR,
            ));
            state.screenshot_size = size;
        }
    }
}
