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
    coord_x: String,
    coord_y: String,
    last_result: String,
    headless: bool,
    screenshot_tex: Option<TextureHandle>,
    screenshot_size: [usize; 2],
    viewport_preset: usize, // 0=Desktop 1=Tablet 2=Mobile
}

impl Default for BrowserUiState {
    fn default() -> Self {
        Self {
            url_input: String::new(),
            selector_input: String::new(),
            type_input: String::new(),
            js_input: String::new(),
            coord_x: String::new(),
            coord_y: String::new(),
            last_result: String::new(),
            headless: true,
            screenshot_tex: None,
            screenshot_size: [0, 0],
            viewport_preset: 0,
        }
    }
}

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut BrowserUiState) {
    let is_open = svc.browser_is_open();
    let mono = egui::TextStyle::Monospace.resolve(ui.style());

    // ── Top bar ──────────────────────────────────────────────────────────
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            // Status dot
            let (color, label) = if is_open { (theme::ACCENT, "LIVE") } else { (theme::TEXT_DIM, "OFF") };
            ui.colored_label(color, RichText::new(label).size(theme::FONT_SMALL).strong());
            ui.add_space(theme::SP_SM);

            // Tab count
            if is_open {
                let tc = svc.browser_tab_count();
                if tc > 1 {
                    theme::badge(ui, &format!("{tc} tabs"), theme::INFO);
                }
            }

            // URL input
            let url_w = (ui.available_width() - 190.0).max(100.0);
            let resp = ui.add_sized(
                [url_w, 24.0],
                egui::TextEdit::singleline(&mut state.url_input)
                    .hint_text("https://...")
                    .font(mono.clone()),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

            if ui.add(egui::Button::new(RichText::new("Go").color(theme::BG))
                .fill(theme::ACCENT).corner_radius(4.0)).clicked() || enter
            {
                if !state.url_input.trim().is_empty() {
                    if !is_open { svc.browser_open(&state.url_input, state.headless); }
                    else { let _ = svc.browser_navigate(&state.url_input); }
                    refresh_screenshot(svc, state, ui.ctx());
                }
            }

            if ui.add(egui::Button::new("Snap").fill(theme::CARD).corner_radius(4.0)).clicked() {
                refresh_screenshot(svc, state, ui.ctx());
            }

            if is_open && ui.add(egui::Button::new(RichText::new("X").color(theme::BG))
                .fill(theme::DANGER).corner_radius(4.0)).clicked()
            {
                svc.browser_close();
                state.screenshot_tex = None;
                state.last_result.clear();
            }
        });

        // Second row: headless toggle + viewport presets + current URL
        ui.horizontal(|ui| {
            ui.checkbox(&mut state.headless, RichText::new("Headless").color(theme::TEXT_DIM).size(theme::FONT_SMALL));
            ui.add_space(theme::SP_SM);

            let presets = ["Desktop", "Tablet", "Mobile"];
            for (i, name) in presets.iter().enumerate() {
                let selected = state.viewport_preset == i;
                let btn = egui::Button::new(RichText::new(*name).size(theme::FONT_CAPTION).color(
                    if selected { theme::BG } else { theme::TEXT_DIM }
                )).fill(if selected { theme::INFO } else { theme::CARD }).corner_radius(4.0);
                if ui.add_enabled(is_open, btn).clicked() {
                    state.viewport_preset = i;
                    let (w, h, m) = match i {
                        1 => (768, 1024, false),
                        2 => (375, 812, true),
                        _ => (1280, 800, false),
                    };
                    let _ = svc.browser_set_viewport(w, h, m);
                    refresh_screenshot(svc, state, ui.ctx());
                }
            }

            if let Some(url) = svc.browser_url() {
                ui.add_space(theme::SP_SM);
                ui.colored_label(theme::TEXT_DIM, RichText::new(url).size(theme::FONT_CAPTION));
            }
        });
    });

    ui.add_space(theme::SP_SM);

    // ── Screenshot preview ───────────────────────────────────────────────
    let avail = ui.available_height();
    let preview_h = (avail * 0.50).max(180.0);

    theme::card(ui, |ui| {
        if let Some(tex) = &state.screenshot_tex {
            let [w, h] = state.screenshot_size;
            let aspect = w as f32 / h.max(1) as f32;
            let disp_w = ui.available_width().min(preview_h * aspect);
            let disp_h = disp_w / aspect;
            ScrollArea::both().id_salt("browser_preview").max_height(preview_h).show(ui, |ui| {
                let img_resp = ui.image(egui::load::SizedTexture::new(tex.id(), egui::vec2(disp_w, disp_h)));
                // Click-on-screenshot → fill coordinate fields
                if img_resp.clicked() {
                    if let Some(pos) = img_resp.interact_pointer_pos() {
                        let rel = pos - img_resp.rect.left_top();
                        let scale_x = w as f32 / disp_w;
                        let scale_y = h as f32 / disp_h;
                        state.coord_x = format!("{:.0}", rel.x * scale_x);
                        state.coord_y = format!("{:.0}", rel.y * scale_y);
                    }
                }
            });
        } else {
            ui.vertical_centered(|ui| {
                ui.add_space(preview_h * 0.4);
                ui.colored_label(theme::TEXT_DIM, "Navigate to a page to see a preview");
            });
            ui.add_space(preview_h * 0.3);
        }
    });

    ui.add_space(theme::SP_SM);

    // ── Actions ──────────────────────────────────────────────────────────
    theme::card(ui, |ui| {
        ui.label(RichText::new("ACTIONS").size(theme::FONT_CAPTION).color(theme::TEXT_DIM));
        ui.add_space(theme::SP_SM);

        // Row 1: Selector → Click / Read / Hover / Wait
        ui.horizontal(|ui| {
            ui.label(RichText::new("Sel").size(theme::FONT_SMALL).color(theme::TEXT_DIM));
            ui.add_sized(
                [ui.available_width() - 230.0, 22.0],
                egui::TextEdit::singleline(&mut state.selector_input)
                    .hint_text("#id / .class")
                    .font(mono.clone()),
            );
            for (label, action) in [("Click", "click"), ("Read", "read"), ("Hover", "hover"), ("Wait", "wait")] {
                if ui.add_enabled(is_open, egui::Button::new(label).fill(theme::CARD).corner_radius(4.0)).clicked() {
                    let sel = state.selector_input.clone();
                    state.last_result = match action {
                        "click" => match svc.browser_click(&sel) {
                            Ok(()) => { refresh_screenshot(svc, state, ui.ctx()); format!("Clicked: {sel}") }
                            Err(e) => format!("ERR: {e}"),
                        },
                        "read" => svc.browser_read(&sel).unwrap_or_else(|e| format!("ERR: {e}")),
                        "hover" => match svc.browser_hover(&sel) {
                            Ok(()) => { refresh_screenshot(svc, state, ui.ctx()); format!("Hovered: {sel}") }
                            Err(e) => format!("ERR: {e}"),
                        },
                        "wait" => match svc.browser_wait(&sel, 5000) {
                            Ok(()) => format!("Found: {sel}"),
                            Err(e) => format!("ERR: {e}"),
                        },
                        _ => String::new(),
                    };
                }
            }
        });

        // Row 2: Type text
        ui.horizontal(|ui| {
            ui.label(RichText::new("Txt").size(theme::FONT_SMALL).color(theme::TEXT_DIM));
            ui.add_sized(
                [ui.available_width() - 80.0, 22.0],
                egui::TextEdit::singleline(&mut state.type_input)
                    .hint_text("text to type...")
                    .font(mono.clone()),
            );
            if ui.add_enabled(is_open && !state.selector_input.is_empty(),
                egui::Button::new("Type").fill(theme::CARD).corner_radius(4.0)).clicked()
            {
                state.last_result = match svc.browser_type(&state.selector_input, &state.type_input) {
                    Ok(()) => { let n = state.type_input.len(); state.type_input.clear(); format!("Typed {n} chars") }
                    Err(e) => format!("ERR: {e}"),
                };
            }
        });

        // Row 3: Coordinate click
        ui.horizontal(|ui| {
            ui.label(RichText::new("XY ").size(theme::FONT_SMALL).color(theme::TEXT_DIM));
            ui.add_sized([60.0, 22.0], egui::TextEdit::singleline(&mut state.coord_x).hint_text("x").font(mono.clone()));
            ui.add_sized([60.0, 22.0], egui::TextEdit::singleline(&mut state.coord_y).hint_text("y").font(mono.clone()));
            if ui.add_enabled(is_open, egui::Button::new("Click XY").fill(theme::CARD).corner_radius(4.0)).clicked() {
                let x: f64 = state.coord_x.parse().unwrap_or(0.0);
                let y: f64 = state.coord_y.parse().unwrap_or(0.0);
                state.last_result = match svc.browser_click_point(x, y) {
                    Ok(()) => { refresh_screenshot(svc, state, ui.ctx()); format!("Clicked ({x},{y})") }
                    Err(e) => format!("ERR: {e}"),
                };
            }
            ui.colored_label(theme::TEXT_DIM, RichText::new("(click screenshot to fill)").size(theme::FONT_CAPTION));
        });

        // Row 4: JS eval
        ui.horizontal(|ui| {
            ui.label(RichText::new("JS ").size(theme::FONT_SMALL).color(theme::TEXT_DIM));
            ui.add_sized(
                [ui.available_width() - 80.0, 22.0],
                egui::TextEdit::singleline(&mut state.js_input)
                    .hint_text("document.title")
                    .font(mono.clone()),
            );
            if ui.add_enabled(is_open, egui::Button::new("Eval").fill(theme::CARD).corner_radius(4.0)).clicked() {
                state.last_result = svc.browser_eval(&state.js_input).unwrap_or_else(|e| format!("ERR: {e}"));
            }
        });

        // Row 5: Key press + Scroll
        ui.horizontal(|ui| {
            if ui.add_enabled(is_open, egui::Button::new("Enter").fill(theme::CARD).corner_radius(4.0)).clicked() {
                let _ = svc.browser_press_key("Enter");
                refresh_screenshot(svc, state, ui.ctx());
            }
            if ui.add_enabled(is_open, egui::Button::new("Tab").fill(theme::CARD).corner_radius(4.0)).clicked() {
                let _ = svc.browser_press_key("Tab");
            }
            if ui.add_enabled(is_open, egui::Button::new("Esc").fill(theme::CARD).corner_radius(4.0)).clicked() {
                let _ = svc.browser_press_key("Escape");
            }
            ui.add_space(theme::SP_SM);
            if ui.add_enabled(is_open, egui::Button::new("Scroll Down").fill(theme::CARD).corner_radius(4.0)).clicked() {
                let _ = svc.browser_scroll(0.0, 400.0);
                refresh_screenshot(svc, state, ui.ctx());
            }
            if ui.add_enabled(is_open, egui::Button::new("Scroll Up").fill(theme::CARD).corner_radius(4.0)).clicked() {
                let _ = svc.browser_scroll(0.0, -400.0);
                refresh_screenshot(svc, state, ui.ctx());
            }
        });

        // ── Result display ───────────────────────────────────────────────
        if !state.last_result.is_empty() {
            ui.add_space(theme::SP_SM);
            ui.separator();
            ui.add_space(theme::SP_SM);
            ScrollArea::vertical().id_salt("browser_result").max_height(80.0).show(ui, |ui| {
                ui.label(RichText::new(&state.last_result).font(mono).color(theme::TEXT));
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
