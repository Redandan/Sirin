//! MCP Playground — standalone panel for browsing and executing external MCP tools.
//! Moved from Settings so it's a first-class UI destination.
//!
//! Layout:
//!   ┌── search ────────────────────────────────┐
//!   │  🔍 [filter by name…]  [server▾]         │
//!   ├──────────────────────────────────────────┤
//!   │  ▶ tool_name   short description          │
//!   │    params: foo:string bar:number          │
//!   │    [{ … }]  [▶ 執行]                       │
//!   │    result …                               │
//!   └──────────────────────────────────────────┘

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use crate::ui_service::{AppService, McpToolDetail};
use super::theme;

// ── State ─────────────────────────────────────────────────────────────────────

pub struct McpPlaygroundState {
    tools:        Vec<McpToolDetail>,
    tools_loaded: bool,
    search:       String,
    server_filter: String,
    expanded:     Option<String>, // registry_name of expanded tool
    args:         String,
    result:       String,
}

impl Default for McpPlaygroundState {
    fn default() -> Self {
        Self {
            tools:        Vec::new(),
            tools_loaded: false,
            search:       String::new(),
            server_filter: String::new(),
            expanded:     None,
            args:         "{}".to_string(),
            result:       String::new(),
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, state: &mut McpPlaygroundState) {
    if !state.tools_loaded {
        state.tools       = svc.mcp_tools();
        state.tools_loaded = true;
    }

    // ── Header ────────────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.colored_label(theme::TEXT,
            RichText::new("MCP PLAYGROUND").size(theme::FONT_TITLE).strong());
        ui.add_space(theme::SP_SM);
        ui.colored_label(theme::TEXT_DIM,
            RichText::new(format!("{} tools", state.tools.len()))
                .size(theme::FONT_SMALL).monospace());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(
                RichText::new("↻").size(theme::FONT_CAPTION).color(theme::TEXT_DIM)
            ).frame(false)).clicked() {
                state.tools        = svc.mcp_tools();
                state.tools_loaded = true;
            }
        });
    });
    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);
    ui.add_space(theme::SP_SM);

    // ── Filter bar ────────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.colored_label(theme::TEXT_DIM,
            RichText::new("🔍").size(theme::FONT_SMALL));
        ui.add(egui::TextEdit::singleline(&mut state.search)
            .hint_text("filter by name…")
            .desired_width(220.0)
            .font(egui::TextStyle::Monospace));

        ui.add_space(theme::SP_SM);

        // Server dropdown.
        let servers: Vec<String> = {
            let mut s: Vec<String> = state.tools.iter()
                .map(|t| t.server_name.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter().collect();
            s.sort();
            s.insert(0, "All".to_string());
            s
        };
        let sel_label = if state.server_filter.is_empty() { "All".to_string() }
            else { state.server_filter.clone() };
        egui::ComboBox::from_id_salt("mcp_server_filter")
            .width(140.0)
            .selected_text(sel_label)
            .show_ui(ui, |ui| {
                for srv in &servers {
                    let val = if srv == "All" { String::new() } else { srv.clone() };
                    ui.selectable_value(&mut state.server_filter, val, srv);
                }
            });

        if (!state.search.is_empty() || !state.server_filter.is_empty())
            && ui.add(egui::Button::new(
                RichText::new("✕").size(theme::FONT_CAPTION)
            )).clicked() {
                state.search.clear();
                state.server_filter.clear();
            }
    });
    ui.add_space(theme::SP_SM);

    // ── Tool list ─────────────────────────────────────────────────────────
    let needle = state.search.to_lowercase();
    let filtered: Vec<&McpToolDetail> = state.tools.iter()
        .filter(|t| {
            (state.server_filter.is_empty() || t.server_name == state.server_filter)
            && (needle.is_empty()
                || t.tool_name.to_lowercase().contains(&needle)
                || t.description.to_lowercase().contains(&needle))
        })
        .collect();

    // Group by server.
    let mut grouped: std::collections::BTreeMap<&str, Vec<&McpToolDetail>> = Default::default();
    for t in &filtered {
        grouped.entry(t.server_name.as_str()).or_default().push(t);
    }

    ScrollArea::vertical().id_salt("mcp_playground").show(ui, |ui| {
        if filtered.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.colored_label(theme::TEXT_DIM, "No tools match filter.");
            });
            return;
        }

        for (server, tools) in &grouped {
            // Server group header.
            ui.colored_label(theme::TEXT_DIM,
                RichText::new(format!("── {} ({}) ──", server, tools.len()))
                    .size(theme::FONT_CAPTION).strong().monospace());
            ui.add_space(theme::SP_XS);

            for tool in tools {
                let is_expanded = state.expanded.as_deref() == Some(&tool.registry_name);

                egui::Frame::new()
                    .fill(theme::CARD)
                    .corner_radius(4.0)
                    .inner_margin(egui::vec2(theme::SP_MD, theme::SP_SM))
                    .show(ui, |ui| {
                        // Tool header row.
                        ui.horizontal(|ui| {
                            let arrow = if is_expanded { "▼" } else { "▶" };
                            if ui.add(egui::Button::new(
                                RichText::new(arrow).size(theme::FONT_CAPTION).color(theme::TEXT_DIM)
                            ).frame(false)).clicked() {
                                if is_expanded {
                                    state.expanded = None;
                                } else {
                                    state.expanded = Some(tool.registry_name.clone());
                                    state.args     = "{}".to_string();
                                    state.result.clear();
                                }
                            }
                            ui.colored_label(theme::INFO,
                                RichText::new(&tool.tool_name).size(theme::FONT_BODY));
                            ui.colored_label(theme::TEXT_DIM,
                                RichText::new(&tool.description).size(theme::FONT_CAPTION));
                        });

                        // Expanded: params + exec.
                        if is_expanded {
                            ui.add_space(theme::SP_XS);
                            if !tool.params.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    for (name, typ) in &tool.params {
                                        theme::badge(ui, &format!("{name}:{typ}"), theme::TEXT_DIM);
                                    }
                                });
                                ui.add_space(theme::SP_XS);
                            }
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [ui.available_width() - 64.0, 24.0],
                                    egui::TextEdit::singleline(&mut state.args)
                                        .font(egui::TextStyle::Monospace)
                                        .hint_text("{ … }"),
                                );
                                if ui.add(egui::Button::new(
                                    RichText::new("▶ Run").size(theme::FONT_SMALL).color(theme::BG)
                                ).fill(theme::INFO).corner_radius(4.0)).clicked() {
                                    state.result = svc.mcp_call(
                                        &tool.registry_name, &state.args
                                    ).unwrap_or_else(|e| format!("❌ {e}"));
                                }
                            });
                            if !state.result.is_empty() {
                                ui.add_space(theme::SP_XS);
                                egui::Frame::new()
                                    .fill(theme::BG)
                                    .corner_radius(4.0)
                                    .inner_margin(theme::SP_SM)
                                    .show(ui, |ui| {
                                        ScrollArea::vertical()
                                            .id_salt("mcp_result")
                                            .max_height(180.0)
                                            .show(ui, |ui| {
                                                ui.colored_label(theme::ACCENT,
                                                    RichText::new(&state.result)
                                                        .size(theme::FONT_SMALL).monospace());
                                            });
                                    });
                            }
                        }
                    });
                ui.add_space(theme::SP_XS);
            }
            ui.add_space(theme::SP_SM);
        }
    });
}
