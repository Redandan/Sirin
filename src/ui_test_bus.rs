//! Cross-thread bus for driving + observing the egui UI from outside.
//!
//! Lets the MCP server (or any caller in the same process) push view-switch
//! commands and read back current UI state — used by automated UI smoke
//! tests because winit's input layer ignores synthesized OS-level keystrokes
//! (SendKeys, keybd_event, mouse_event), which makes external Win32-style
//! UI automation unreliable.
//!
//! Architecture:
//!   • MCP thread → `push(UiCommand)` → wakes egui via `ctx.request_repaint()`
//!   • egui `update()` → `drain()` → applies each command → writes back via `set_state(...)`
//!   • Any thread → `get_state()` reads the most recent snapshot
//!
//! This is a TEST/DEV surface only — production users wouldn't normally
//! invoke `ui_navigate` from outside. No auth required since the MCP
//! server already binds to 127.0.0.1.

use std::sync::{Mutex, OnceLock};

// ── Commands ─────────────────────────────────────────────────────────────────

/// A single UI mutation request. Cheap to clone, sent across threads.
#[derive(Debug, Clone)]
pub enum UiCommand {
    /// Switch to the Dashboard view; close any open palette/modal/gear menu.
    GoDashboard,
    /// Switch to the Testing view; `tab` is "runs" | "coverage" | "browser".
    GoTesting { tab: String },
    /// Switch to the per-agent workspace at index `idx`.
    GoWorkspace { idx: usize },
    /// Open the ⌘K command palette, optionally with `query` pre-filled.
    OpenPalette { query: Option<String> },
    /// Close the command palette.
    ClosePalette,
    /// Open a modal panel.
    /// `kind` is "automation" | "ops" | "system";
    /// `tab` is the inner sub-tab — "squad" | "mcp" | "airouter" |
    /// "sessiontasks" | "costkb" | "settings" | "log".
    OpenModal { kind: String, tab: Option<String> },
    /// Close any open modal.
    CloseModal,
    /// Open the gear dropdown menu.
    OpenGearMenu,
    /// Close the gear dropdown menu.
    CloseGearMenu,
}

// ── State snapshot ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct UiSnapshot {
    /// "Dashboard" | "Testing" | "Workspace"
    pub view:           String,
    /// Workspace agent index (only when view == "Workspace").
    pub workspace_idx:  Option<usize>,
    /// Testing inner tab — "Runs" | "Coverage" | "Browser" — when view == "Testing".
    pub testing_tab:    Option<String>,
    /// "None" | "Automation" | "Ops" | "System"
    pub modal:          String,
    /// Inner sub-tab of whichever modal is open, when one is open.
    pub modal_tab:      Option<String>,
    pub palette_open:   bool,
    pub palette_query:  String,
    pub gear_menu_open: bool,
    pub agent_count:    usize,
    pub active_runs:    usize,
    pub recent_runs:    usize,
}

// ── Bus internals ────────────────────────────────────────────────────────────

struct Bus {
    queue:      Vec<UiCommand>,
    last_state: Option<UiSnapshot>,
    /// egui Context — set once on first SirinApp::new(). Lets us call
    /// `request_repaint()` from MCP threads so commands are processed
    /// promptly instead of waiting for the next 5-second poll tick.
    ctx:        Option<eframe::egui::Context>,
}

fn bus() -> &'static Mutex<Bus> {
    static B: OnceLock<Mutex<Bus>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(Bus {
        queue:      Vec::new(),
        last_state: None,
        ctx:        None,
    }))
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Called once by SirinApp::new() to give the bus a handle for repaint requests.
pub fn register_ctx(ctx: eframe::egui::Context) {
    if let Ok(mut b) = bus().lock() {
        b.ctx = Some(ctx);
    }
}

/// Push a command. Fires a repaint request so egui processes it on the next frame.
pub fn push(cmd: UiCommand) {
    if let Ok(mut b) = bus().lock() {
        b.queue.push(cmd);
        if let Some(ctx) = &b.ctx {
            ctx.request_repaint();
        }
    }
}

/// Drain queued commands. Called by egui `update()`.
pub fn drain() -> Vec<UiCommand> {
    bus().lock().map(|mut b| std::mem::take(&mut b.queue)).unwrap_or_default()
}

/// egui `update()` writes its current visible state here at end of each frame.
pub fn set_state(s: UiSnapshot) {
    if let Ok(mut b) = bus().lock() {
        b.last_state = Some(s);
    }
}

/// Read the most recent state snapshot. Returns None until egui has rendered once.
pub fn get_state() -> Option<UiSnapshot> {
    bus().lock().ok().and_then(|b| b.last_state.clone())
}
