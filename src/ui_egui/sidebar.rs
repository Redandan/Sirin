//! Sidebar — icon-only narrow nav (Linear/Notion style) + optional labelled mode.
//!
//! Default mode (52px wide):
//!   ┌────┐
//!   │ S  │   ← Logo badge (ACCENT fill)
//!   │    │
//!   │ A1 │   ← Agent dots (first-letter, status colored)
//!   │ A2 │
//!   │    │
//!   │ ⚙  │   ← System / tool icons
//!   │ ☰  │
//!   │ ◎  │
//!   │ ▣  │
//!   │ ⚇  │
//!   │    │
//!   │ ›  │   ← expand to labelled mode
//!   │ ●● │   ← TG / RPC status dots
//!   └────┘
//!
//! Labelled mode (210px) — accordion sections:
//!   ▼ AGENTS / ▶ SYSTEM / ▶ TOOLS
//!
//! Hover any icon → tooltip with full label.

use std::sync::Arc;
use eframe::egui::{self, RichText, ScrollArea};
use super::{View, theme};
use crate::ui_service::{AgentSummary, AppService};

// ── Icon dispatch ─────────────────────────────────────────────────────────────

/// SVG-style icons drawn with Painter primitives (line art, not glyphs).
/// `Glyph` is the escape hatch for agent first-letter buttons + universal
/// symbols like ⚙ that don't benefit from re-drawing.
enum Icon<'a> {
    Glyph(&'a str),
    Lines,    // log — three horizontal lines
    Globe,    // browser — circle + meridian + equator + latitude bands
    Monitor,  // monitor — screen rect + stand + base
    People,   // team — front person + two heads peeking
    Gear,     // settings — outer gear teeth + inner ring
}

/// Draw an icon centered at `c`, sized to fit a ~16px box.
fn draw_icon(painter: &egui::Painter, c: egui::Pos2, icon: &Icon, color: egui::Color32) {
    match icon {
        Icon::Glyph(s) => {
            painter.text(c, egui::Align2::CENTER_CENTER, *s,
                egui::FontId::proportional(16.0), color);
        }
        Icon::Lines   => draw_lines(painter, c, color),
        Icon::Globe   => draw_globe(painter, c, color),
        Icon::Monitor => draw_monitor(painter, c, color),
        Icon::People  => draw_people(painter, c, color),
        Icon::Gear    => draw_gear(painter, c, color),
    }
}

/// Three horizontal lines — log / list icon.
fn draw_lines(painter: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let s = egui::Stroke::new(1.4, col);
    let half_w = 6.5_f32;
    for dy in [-4.0_f32, 0.0, 4.0] {
        painter.line_segment(
            [egui::pos2(c.x - half_w, c.y + dy), egui::pos2(c.x + half_w, c.y + dy)],
            s,
        );
    }
}

/// Globe — outer circle with meridian + equator + 2 latitude bands.
fn draw_globe(painter: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let s = egui::Stroke::new(1.4, col);
    let r = 7.0_f32;
    // Outer sphere
    painter.circle_stroke(c, r, s);
    // Meridian (vertical)
    painter.line_segment([egui::pos2(c.x, c.y - r), egui::pos2(c.x, c.y + r)], s);
    // Equator (horizontal)
    painter.line_segment([egui::pos2(c.x - r, c.y), egui::pos2(c.x + r, c.y)], s);
    // Two latitude bands at y = ±3.5 (chord width = sqrt(r² − 3.5²))
    let dy = 3.5_f32;
    let dx = (r * r - dy * dy).sqrt();
    painter.line_segment(
        [egui::pos2(c.x - dx, c.y - dy), egui::pos2(c.x + dx, c.y - dy)], s,
    );
    painter.line_segment(
        [egui::pos2(c.x - dx, c.y + dy), egui::pos2(c.x + dx, c.y + dy)], s,
    );
}

/// Monitor — screen rect (drawn from 4 line segments) + stand + base.
fn draw_monitor(painter: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let s = egui::Stroke::new(1.4, col);
    // Screen: 14 wide × 9 tall, sitting just above center
    let screen = egui::Rect::from_center_size(
        egui::pos2(c.x, c.y - 1.5),
        egui::vec2(14.0, 9.0),
    );
    painter.line_segment([screen.left_top(),    screen.right_top()],    s);
    painter.line_segment([screen.right_top(),   screen.right_bottom()], s);
    painter.line_segment([screen.right_bottom(),screen.left_bottom()],  s);
    painter.line_segment([screen.left_bottom(), screen.left_top()],     s);
    // Stand (vertical neck)
    painter.line_segment(
        [egui::pos2(c.x, screen.bottom()), egui::pos2(c.x, c.y + 5.5)], s,
    );
    // Base (foot)
    painter.line_segment(
        [egui::pos2(c.x - 4.5, c.y + 6.0), egui::pos2(c.x + 4.5, c.y + 6.0)], s,
    );
}

/// People — front figure (head + shoulders) + two smaller heads peeking behind.
fn draw_people(painter: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let s = egui::Stroke::new(1.4, col);

    // Back-left head (small, slightly higher offset)
    painter.circle_stroke(egui::pos2(c.x - 5.5, c.y - 0.8), 1.7, s);
    // Back-right head
    painter.circle_stroke(egui::pos2(c.x + 5.5, c.y - 0.8), 1.7, s);

    // Front person head (larger, lower)
    painter.circle_stroke(egui::pos2(c.x, c.y - 2.5), 2.4, s);

    // Front shoulders/torso — trapezoid drawn with 3 line segments
    let l = egui::pos2(c.x - 4.0, c.y + 6.0);
    let lt = egui::pos2(c.x - 3.4, c.y + 1.8);
    let rt = egui::pos2(c.x + 3.4, c.y + 1.8);
    let r = egui::pos2(c.x + 4.0, c.y + 6.0);
    painter.line_segment([l, lt], s);
    painter.line_segment([lt, rt], s);
    painter.line_segment([rt, r], s);
}

/// Gear — outer ring + 6 short radial teeth + inner hole.
fn draw_gear(painter: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let s = egui::Stroke::new(1.4, col);
    let r_outer = 5.5_f32;
    let r_tooth = 7.2_f32;
    // Outer ring
    painter.circle_stroke(c, r_outer, s);
    // Inner hole
    painter.circle_stroke(c, 2.0, s);
    // 6 radial teeth (short tick marks at 60° intervals)
    for i in 0..6 {
        let theta = (i as f32) * std::f32::consts::TAU / 6.0;
        let (sn, cs) = theta.sin_cos();
        painter.line_segment(
            [
                egui::pos2(c.x + cs * r_outer, c.y + sn * r_outer),
                egui::pos2(c.x + cs * r_tooth, c.y + sn * r_tooth),
            ],
            s,
        );
    }
}

// ── Sidebar state ─────────────────────────────────────────────────────────────

/// `expanded`=false → icon-only narrow strip (default, like the reference image).
/// `expanded`=true  → labelled accordion mode.
pub struct SidebarState {
    pub expanded:       bool,
    pub agents_open:    bool,
    pub testing_open:   bool,
    pub automation_open: bool,
    pub ops_open:       bool,
    pub system_open:    bool,
    // legacy — kept for ensure_view_visible compat
    pub tools_open:     bool,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            expanded:        false,
            agents_open:     true,
            testing_open:    true,
            automation_open: false,
            ops_open:        false,
            system_open:     false,
            tools_open:      false,
        }
    }
}

impl SidebarState {
    fn ensure_view_visible(&mut self, view: &View) {
        match view {
            View::Workspace(_) => self.agents_open = true,
            View::Settings | View::Log => self.system_open = true,
            View::TestRuns | View::Coverage | View::BrowserMonitor => self.testing_open = true,
            View::Team | View::McpPlayground => self.automation_open = true,
            View::AiRouter | View::SessionTasks | View::CostKb => self.ops_open = true,
        }
    }
}

pub fn show(
    ctx: &egui::Context, svc: &Arc<dyn AppService>, agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View, renaming: &mut Option<(usize, String)>,
    state: &mut SidebarState,
) {
    let panel_w = if state.expanded { 210.0_f32 } else { 52.0 };
    let margin  = if state.expanded { theme::SP_SM } else { 0.0 };

    egui::SidePanel::left("sidebar").resizable(false).exact_width(panel_w)
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::vec2(margin, 0.0)))
        .show(ctx, |ui| {
            if state.expanded {
                show_expanded(ui, svc, agents, pending_counts, view, renaming, state);
            } else {
                show_icon_nav(ui, svc, agents, pending_counts, view, state);
            }
        });
}

// ── Icon-only nav (52 px) ────────────────────────────────────────────────────

fn show_icon_nav(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View,
    state: &mut SidebarState,
) {
    ui.add_space(theme::SP_SM);

    // ── Top: Sirin logo badge ─────────────────────────────────────────
    draw_logo_badge(ui);
    ui.add_space(theme::SP_SM);
    thin_strip(ui);
    ui.add_space(theme::SP_XS);

    // ── Agents: scrollable column of dots ─────────────────────────────
    // Reserve room for the system tools + bottom controls so the agent
    // list doesn't push them off-screen on a small window.
    let bottom_reserve = 220.0;
    ScrollArea::vertical().id_salt("icon_agents").max_height(
        (ui.available_height() - bottom_reserve).max(60.0),
    ).show(ui, |ui| {
        for (idx, agent) in agents.iter().enumerate() {
            let active = matches!(view, View::Workspace(i) if *i == idx);
            let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
            let dot_color = match agent.live_status.as_str() {
                "connected" => theme::ACCENT,
                "reconnecting" | "waiting" => theme::YELLOW,
                "error" => theme::DANGER,
                _ => if agent.enabled { theme::TEXT_DIM } else { theme::BORDER },
            };
            let initial = agent.name.chars().next().unwrap_or('•').to_string();
            let tooltip = format!("{} ({}) — {}", agent.name, agent.platform, agent.live_status);
            if icon_button(ui, Icon::Glyph(&initial), active, Some(dot_color), pending_n, &tooltip) {
                *view = View::Workspace(idx);
            }
        }
    });

    ui.add_space(theme::SP_XS);
    thin_strip(ui);
    ui.add_space(theme::SP_XS);

    // ── TESTING ──────────────────────────────────────────────────────────
    nav_icon(ui, Icon::Lines,   "Test Runs",      View::TestRuns,       view);
    nav_icon(ui, Icon::Glyph("▦"), "Coverage",   View::Coverage,       view);
    nav_icon(ui, Icon::Globe,   "Browser",        View::BrowserMonitor, view);

    ui.add_space(theme::SP_XS);
    thin_strip(ui);
    ui.add_space(theme::SP_XS);

    // ── AUTOMATION ───────────────────────────────────────────────────────
    nav_icon(ui, Icon::People,  "Dev Squad",      View::Team,          view);
    nav_icon(ui, Icon::Glyph("⚙"), "MCP",        View::McpPlayground, view);

    ui.add_space(theme::SP_XS);
    thin_strip(ui);
    ui.add_space(theme::SP_XS);

    // ── OPS ──────────────────────────────────────────────────────────────
    nav_icon(ui, Icon::Glyph("⚡"), "AI Router",  View::AiRouter,     view);
    nav_icon(ui, Icon::Glyph("📋"), "Tasks",      View::SessionTasks, view);
    nav_icon(ui, Icon::Glyph("$"),  "Cost & KB",  View::CostKb,       view);

    ui.add_space(theme::SP_XS);
    thin_strip(ui);
    ui.add_space(theme::SP_XS);

    // ── SYSTEM ───────────────────────────────────────────────────────────
    nav_icon(ui, Icon::Gear,    "Settings",       View::Settings, view);
    nav_icon(ui, Icon::Monitor, "Log",            View::Log,      view);

    // ── Bottom: expand button + status dots ──────────────────────────
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
        ui.add_space(theme::SP_SM);

        let s = svc.system_status();
        // RPC dot
        let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), egui::Sense::hover());
        ui.painter().circle_filled(r.center(), 3.5, if s.rpc_running { theme::ACCENT } else { theme::DANGER });
        resp.on_hover_text(if s.rpc_running { "RPC: running" } else { "RPC: stopped" });
        ui.add_space(5.0);

        // Telegram dot
        let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), egui::Sense::hover());
        ui.painter().circle_filled(r.center(), 3.5, if s.telegram_connected { theme::ACCENT } else { theme::DANGER });
        resp.on_hover_text(if s.telegram_connected { "Telegram: connected" } else { "Telegram: offline" });

        ui.add_space(theme::SP_SM);
        thin_strip(ui);
        ui.add_space(theme::SP_XS);

        // Expand button (›)
        let cw = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(cw, 28.0), egui::Sense::click());
        let col = if resp.hovered() { theme::TEXT } else { theme::TEXT_DIM };
        if resp.hovered() { ui.painter().rect_filled(rect, 4.0, theme::CARD); }
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "›",
            egui::FontId::proportional(18.0), col);
        if resp.clicked() { state.expanded = true; }
        resp.on_hover_text("Expand sidebar");
    });
}

/// Sirin "S" badge — accent-colored rounded square at the very top.
fn draw_logo_badge(ui: &mut egui::Ui) {
    let cw = ui.available_width();
    let (row_rect, _) = ui.allocate_exact_size(egui::vec2(cw, 36.0), egui::Sense::hover());
    let badge_size = 32.0_f32;
    let center = row_rect.center();
    let badge_rect = egui::Rect::from_center_size(center, egui::vec2(badge_size, badge_size));
    ui.painter().rect_filled(badge_rect, 6.0, theme::ACCENT);
    ui.painter().text(
        badge_rect.center(), egui::Align2::CENTER_CENTER, "S",
        egui::FontId::proportional(18.0),
        theme::BG,
    );
}

/// Generic nav icon (system / tool) — shows tooltip and active highlight.
fn nav_icon(ui: &mut egui::Ui, icon: Icon<'_>, label: &str, target: View, current: &mut View) {
    let active = std::mem::discriminant(current) == std::mem::discriminant(&target);
    if icon_button(ui, icon, active, None, 0, label) {
        *current = target;
    }
}

/// Single icon nav button — 40x40 centered in the panel.
/// `active` → filled HOVER bg + ACCENT left bar (matches reference image).
/// `hover` → faint CARD bg.
/// `dot_color` → small status dot at top-right (for agents).
/// `badge_n` → pending count overlay at bottom-right.
fn icon_button(
    ui: &mut egui::Ui,
    icon: Icon<'_>,
    active: bool,
    dot_color: Option<egui::Color32>,
    badge_n: usize,
    tooltip: &str,
) -> bool {
    let cw = ui.available_width();
    let (row_rect, resp) = ui.allocate_exact_size(egui::vec2(cw, 40.0), egui::Sense::click());

    let btn_size = 36.0_f32;
    let btn_rect = egui::Rect::from_center_size(row_rect.center(), egui::vec2(btn_size, btn_size));

    if active {
        ui.painter().rect_filled(btn_rect, 6.0, theme::HOVER);
        // Accent left bar — visual anchor for the active item.
        let bar = egui::Rect::from_min_size(
            egui::pos2(row_rect.left(), btn_rect.top() + 6.0),
            egui::vec2(3.0, btn_rect.height() - 12.0),
        );
        ui.painter().rect_filled(bar, 1.5, theme::ACCENT);
    } else if resp.hovered() {
        ui.painter().rect_filled(btn_rect, 6.0, theme::CARD);
    }

    let text_color = if active { theme::TEXT } else { theme::TEXT_DIM };
    draw_icon(ui.painter(), btn_rect.center(), &icon, text_color);

    // Status dot (top-right)
    if let Some(c) = dot_color {
        let pos = egui::pos2(btn_rect.right() - 4.0, btn_rect.top() + 4.0);
        ui.painter().circle_filled(pos, 3.0, c);
    }

    // Pending badge (bottom-right)
    if badge_n > 0 {
        let pos = egui::pos2(btn_rect.right() - 2.0, btn_rect.bottom() - 6.0);
        ui.painter().circle_filled(pos, 5.5, theme::ACCENT);
        ui.painter().text(
            pos, egui::Align2::CENTER_CENTER, format!("{badge_n}"),
            egui::FontId::proportional(8.5), theme::BG,
        );
    }

    let clicked = resp.clicked();
    resp.on_hover_text(tooltip);
    clicked
}

/// Hairline horizontal divider centered in the strip.
fn thin_strip(ui: &mut egui::Ui) {
    let cw = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(cw, 1.0), egui::Sense::hover());
    let inset = 8.0_f32;
    ui.painter().line_segment(
        [egui::pos2(rect.left() + inset, rect.center().y),
         egui::pos2(rect.right() - inset, rect.center().y)],
        egui::Stroke::new(0.5, theme::BORDER),
    );
}

// ── Labelled mode (210 px, accordion) ────────────────────────────────────────

fn show_expanded(
    ui: &mut egui::Ui,
    svc: &Arc<dyn AppService>,
    agents: &[AgentSummary],
    pending_counts: &std::collections::HashMap<String, usize>,
    view: &mut View,
    renaming: &mut Option<(usize, String)>,
    state: &mut SidebarState,
) {
    state.ensure_view_visible(view);

    ui.add_space(theme::SP_SM);

    // AGENTS header + collapse button (›‹)
    ui.horizontal(|ui| {
        ui.add_space(theme::SP_MD);
        group_header_inline(ui, "AGENTS", &mut state.agents_open);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
            let col = if resp.hovered() { theme::TEXT } else { theme::TEXT_DIM };
            if resp.hovered() { ui.painter().rect_filled(rect, 3.0, theme::CARD); }
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "‹",
                egui::FontId::proportional(18.0), col);
            if resp.clicked() { state.expanded = false; }
        });
    });
    ui.add_space(theme::SP_XS);

    let bottom_reserved = 200.0;
    if state.agents_open {
        ScrollArea::vertical().id_salt("agents")
            .max_height((ui.available_height() - bottom_reserved).max(80.0))
            .show(ui, |ui| {
                let mut rename_commit: Option<(usize, String)> = None;
                for (idx, agent) in agents.iter().enumerate() {
                    let is_selected = matches!(view, View::Workspace(i) if *i == idx);
                    let pending_n = pending_counts.get(&agent.id).copied().unwrap_or(0);
                    let is_renaming = renaming.as_ref().map(|(i, _)| *i == idx).unwrap_or(false);

                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 28.0),
                        egui::Sense::click(),
                    );

                    if is_selected {
                        ui.painter().rect_filled(rect, 4.0, theme::HOVER);
                        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height()));
                        ui.painter().rect_filled(bar, 1.0, theme::ACCENT);
                    } else if response.hovered() {
                        ui.painter().rect_filled(rect, 4.0, theme::CARD);
                    }

                    let inner = rect.shrink2(egui::vec2(theme::SP_SM, 0.0));

                    if is_renaming {
                        let buf = &mut renaming.as_mut().unwrap().1;
                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
                        let resp = child.text_edit_singleline(buf);
                        if resp.lost_focus() {
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                rename_commit = Some((idx, buf.clone()));
                            }
                            *renaming = None;
                        }
                        resp.request_focus();
                    } else {
                        if response.clicked() { *view = View::Workspace(idx); }
                        if response.double_clicked() { *renaming = Some((idx, agent.name.clone())); }
                        response.on_hover_text(format!("{} ({}) — {}", agent.name, agent.platform, agent.live_status));

                        let dot_color = match agent.live_status.as_str() {
                            "connected" => theme::ACCENT,
                            "reconnecting" | "waiting" => theme::YELLOW,
                            "error" => theme::DANGER,
                            _ => if agent.enabled { theme::TEXT_DIM } else { theme::BORDER },
                        };
                        let dot_center = egui::pos2(inner.left() + 8.0, inner.center().y);
                        ui.painter().circle_filled(dot_center, 3.0, dot_color);

                        let text_color = if is_selected { theme::TEXT } else { theme::TEXT_DIM };
                        let name_pos = egui::pos2(inner.left() + 20.0, inner.center().y - 6.5);
                        ui.painter().text(
                            name_pos, egui::Align2::LEFT_TOP, &agent.name,
                            egui::FontId::proportional(theme::FONT_BODY), text_color,
                        );

                        if pending_n > 0 {
                            let pos = egui::pos2(inner.right() - 4.0, inner.center().y);
                            ui.painter().text(
                                pos, egui::Align2::RIGHT_CENTER, format!("{pending_n}"),
                                egui::FontId::proportional(theme::FONT_CAPTION), theme::ACCENT,
                            );
                        }
                    }
                }
                if let Some((idx, name)) = rename_commit {
                    if let Some(agent) = agents.get(idx) { svc.rename_agent(&agent.id, &name); }
                }
            });
    }

    ui.add_space(theme::SP_SM);
    theme::thin_separator(ui);

    // ── TESTING ──────────────────────────────────────────────────────────
    if group_header(ui, "TESTING", state.testing_open) {
        state.testing_open = !state.testing_open;
    }
    if state.testing_open {
        nav_item(ui, "Test Runs",    View::TestRuns,       view);
        nav_item(ui, "Coverage",     View::Coverage,       view);
        nav_item(ui, "Browser",      View::BrowserMonitor, view);
    }

    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);

    // ── AUTOMATION ───────────────────────────────────────────────────────
    if group_header(ui, "AUTOMATION", state.automation_open) {
        state.automation_open = !state.automation_open;
    }
    if state.automation_open {
        nav_item(ui, "Dev Squad",      View::Team,          view);
        nav_item(ui, "MCP Playground", View::McpPlayground, view);
    }

    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);

    // ── OPS ──────────────────────────────────────────────────────────────
    if group_header(ui, "OPS", state.ops_open) {
        state.ops_open = !state.ops_open;
    }
    if state.ops_open {
        nav_item(ui, "AI Router",        View::AiRouter,     view);
        nav_item(ui, "Session & Tasks",  View::SessionTasks, view);
        nav_item(ui, "Cost & KB",        View::CostKb,       view);
    }

    ui.add_space(theme::SP_XS);
    theme::thin_separator(ui);

    // ── SYSTEM ───────────────────────────────────────────────────────────
    if group_header(ui, "SYSTEM", state.system_open) {
        state.system_open = !state.system_open;
    }
    if state.system_open {
        nav_item(ui, "Settings", View::Settings, view);
        nav_item(ui, "Log",      View::Log,      view);
    }

    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
        ui.add_space(theme::SP_SM);
        let status = svc.system_status();
        ui.horizontal(|ui| {
            ui.add_space(theme::SP_MD);
            status_indicator(ui, "TG", status.telegram_connected);
            ui.add_space(theme::SP_MD);
            status_indicator(ui, "RPC", status.rpc_running);
        });
        ui.add_space(theme::SP_XS);
        theme::thin_separator(ui);
    });
}

fn group_header(ui: &mut egui::Ui, text: &str, open: bool) -> bool {
    let caret = if open { "▼" } else { "▶" };
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.add_space(theme::SP_MD);
        let resp = ui.add(
            egui::Label::new(
                RichText::new(format!("{caret}  {text}"))
                    .size(theme::FONT_CAPTION)
                    .strong()
                    .color(theme::TEXT_DIM),
            )
            .selectable(false)
            .sense(egui::Sense::click()),
        );
        clicked = resp.clicked();
    });
    ui.add_space(theme::SP_XS);
    clicked
}

fn group_header_inline(ui: &mut egui::Ui, text: &str, open: &mut bool) {
    let caret = if *open { "▼" } else { "▶" };
    let resp = ui.add(
        egui::Label::new(
            RichText::new(format!("{caret}  {text}"))
                .size(theme::FONT_CAPTION)
                .strong()
                .color(theme::TEXT_DIM),
        )
        .selectable(false)
        .sense(egui::Sense::click()),
    );
    if resp.clicked() { *open = !*open; }
}

fn nav_item(ui: &mut egui::Ui, label: &str, target: View, current: &mut View) {
    let active = std::mem::discriminant(current) == std::mem::discriminant(&target);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 28.0),
        egui::Sense::click(),
    );

    if active {
        ui.painter().rect_filled(rect, 4.0, theme::HOVER);
        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height()));
        ui.painter().rect_filled(bar, 1.0, theme::ACCENT);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 4.0, theme::CARD);
    }

    let text_color = if active { theme::TEXT } else { theme::TEXT_DIM };
    let pos = egui::pos2(rect.left() + theme::SP_MD + 8.0, rect.center().y - 6.5);
    ui.painter().text(pos, egui::Align2::LEFT_TOP, label,
        egui::FontId::proportional(theme::FONT_BODY), text_color);

    if response.clicked() { *current = target; }
}

fn status_indicator(ui: &mut egui::Ui, label: &str, ok: bool) {
    let color = if ok { theme::ACCENT } else { theme::DANGER };
    ui.colored_label(color, RichText::new("●").size(theme::FONT_CAPTION));
    ui.colored_label(theme::TEXT_DIM, RichText::new(label).size(theme::FONT_CAPTION));
}
