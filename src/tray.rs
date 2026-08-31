//! System tray icon for the Sirin daemon — Issue #272.
//!
//! Sirin became daemon-style in v0.5.0 (closing the web UI tab no longer
//! kills the process).  Without an OS-level affordance the daemon was
//! invisible — users had to `curl :7700/api/snapshot` to know whether
//! Sirin was running, and `taskkill` to stop it.
//!
//! ## Architecture
//!
//! - Spawned on a dedicated `sirin-tray` OS thread via [`spawn_tray`].
//!   Using a thread (rather than a tokio task) because tray-icon needs a
//!   thread-local Win32 window + message pump that conflicts with tokio's
//!   work-stealing scheduler.
//! - The thread runs a non-blocking `PeekMessageW` pump alongside a 5s
//!   tooltip refresh and a `MenuEvent` drain — no GUI event loop dep.
//! - Status (`Running N, Queued M`) comes from a blocking GET against
//!   `http://127.0.0.1:7700/api/snapshot` every 5s.  Failures are silent —
//!   the tooltip stays stale rather than spamming logs.
//! - Skipped entirely in headless mode — see `main.rs` (no GUI session,
//!   tray APIs would error).
//!
//! ## MVP scope (v0.5.7)
//!
//! - Static icon (`icons/32x32.png`, embedded via `include_bytes!`)
//! - Tooltip refresh every 5s
//! - Menu items: Open Dashboard | Quit
//!
//! Deferred: per-status icon colours, pulse animation, more menu items
//! (Open Coverage / Pause / Reload Config / Tail Log).  Open Coverage
//! existed in the v0.5.7 first cut but was pruned in v0.5.8 — `#coverage`
//! is one click away from the dashboard's nav and didn't justify its
//! own tray slot.

use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

const STATUS_REFRESH: Duration = Duration::from_secs(5);
const PUMP_TICK: Duration = Duration::from_millis(150);
const SNAPSHOT_URL: &str = "http://127.0.0.1:7700/api/snapshot";
const DASHBOARD_URL: &str = "http://127.0.0.1:7700/ui/";
const AI_MONITOR_URL: &str = "http://127.0.0.1:7700/ui/?view=ai-monitor";

/// Spawn the tray icon on a dedicated OS thread.  Returns `true` when the
/// thread successfully launched (the tray icon is the discovery affordance
/// for the daemon — caller can use this to suppress redundant browser
/// auto-open on Sirin startup).  Returns `false` on spawn failure.
pub fn spawn_tray() -> bool {
    let result = std::thread::Builder::new()
        .name("sirin-tray".into())
        .spawn(tray_thread_main);

    match result {
        Ok(_) => {
            tracing::info!("[tray] spawned sirin-tray thread");
            true
        }
        Err(e) => {
            tracing::warn!(
                "[tray] failed to spawn tray thread: {e} — daemon continues without tray"
            );
            false
        }
    }
}

fn tray_thread_main() {
    // Build menu before tray icon so item IDs are captured for event matching.
    let tray_menu = Menu::new();
    let open_dashboard = MenuItem::new("Open Dashboard", true, None);
    let open_ai_monitor = MenuItem::new("Open AI Work Monitor", true, None);
    let quit_item = MenuItem::new("Quit Sirin", true, None);

    if tray_menu.append(&open_dashboard).is_err()
        || tray_menu.append(&open_ai_monitor).is_err()
        || tray_menu.append(&PredefinedMenuItem::separator()).is_err()
        || tray_menu.append(&quit_item).is_err()
    {
        tracing::warn!("[tray] failed to append menu items — aborting tray");
        return;
    }

    let icon = match load_icon() {
        Some(i) => i,
        None => {
            tracing::warn!("[tray] failed to load icon — aborting tray");
            return;
        }
    };

    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip(format!(
            "Sirin v{} — initializing",
            env!("CARGO_PKG_VERSION")
        ))
        .with_icon(icon)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("[tray] failed to build tray icon: {e} — daemon continues without tray");
            return;
        }
    };

    tracing::info!("[tray] tray icon active — daemon visible in system notification area");

    // Cache the menu IDs so the event-receive branch doesn't allocate.
    let id_dashboard = open_dashboard.id().clone();
    let id_ai_monitor = open_ai_monitor.id().clone();
    let id_quit = quit_item.id().clone();

    let mut last_status_update = Instant::now() - STATUS_REFRESH;
    let menu_events = MenuEvent::receiver();

    loop {
        pump_messages();

        // Drain all queued menu events (non-blocking).
        while let Ok(event) = menu_events.try_recv() {
            if event.id == id_dashboard {
                let _ = open_url(DASHBOARD_URL);
            } else if event.id == id_ai_monitor {
                let _ = open_url(AI_MONITOR_URL);
            } else if event.id == id_quit {
                tracing::info!("[tray] Quit Sirin clicked — exiting daemon");
                std::process::exit(0);
            }
        }

        if last_status_update.elapsed() >= STATUS_REFRESH {
            update_tooltip(&tray);
            last_status_update = Instant::now();
        }

        std::thread::sleep(PUMP_TICK);
    }
}

/// Run a non-blocking Win32 message pump iteration so tray-icon's
/// internal window can dispatch click / hover events.
#[cfg(windows)]
fn pump_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(not(windows))]
fn pump_messages() {
    // Mac/Linux placeholder — would need NSApplication / glib loop here.
    // MVP is Windows-only; non-Windows build still compiles cleanly so
    // the daemon runs without tray (see spawn_tray docs).
}

/// Decode `icons/32x32.png` (bundled via `include_bytes!`) into the
/// RGBA buffer that tray-icon expects.
fn load_icon() -> Option<tray_icon::Icon> {
    const ICON_BYTES: &[u8] = include_bytes!("../icons/32x32.png");
    let img = image::load_from_memory(ICON_BYTES).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Pull the current snapshot and rebuild the tooltip text.  Failures
/// (Sirin not yet listening, transient network) leave the previous
/// tooltip in place — no log spam.
fn update_tooltip(tray: &TrayIcon) {
    let text = fetch_status_text().unwrap_or_else(|| {
        format!(
            "Sirin v{} — snapshot unreachable",
            env!("CARGO_PKG_VERSION")
        )
    });
    let _ = tray.set_tooltip(Some(&text));
}

/// Synchronous snapshot fetch via `reqwest::blocking` (already pulled in
/// via the `blocking` feature in Cargo.toml).
///
/// #279 tier 1 — tooltip now folds in Phase A/B telemetry that the snapshot
/// already carries: active run subphase, queue drain ETA, browser
/// idle-close countdown.  Keeps the tray useful at a glance without having
/// to open the dashboard for "is anything stuck?".
fn fetch_status_text() -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let resp = client.get(SNAPSHOT_URL).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().ok()?;

    let runs = json.get("active_runs")?.as_array()?;
    let queued = runs
        .iter()
        .filter(|r| r.get("status").and_then(|s| s.as_str()) == Some("queued"))
        .count();
    let running_runs: Vec<&serde_json::Value> = runs
        .iter()
        .filter(|r| r.get("status").and_then(|s| s.as_str()) == Some("running"))
        .collect();
    let running = running_runs.len();
    let version = json.get("version").and_then(|v| v.as_str()).unwrap_or("?");
    let llm_main = json.get("llm_main").and_then(|v| v.as_str()).unwrap_or("?");

    let mut lines = Vec::with_capacity(6);
    lines.push(format!("Sirin v{version}"));
    lines.push(format!("Running {running}, Queued {queued}"));
    lines.push(format!("LLM: {llm_main}"));

    // Per-run sub-phase line — most informative when 1 test is in flight.
    if let Some(r) = running_runs.first() {
        let test_id = r.get("test_id").and_then(|v| v.as_str()).unwrap_or("?");
        let subphase = r.get("current_subphase").and_then(|v| v.as_str());
        let idle_secs = r.get("idle_secs").and_then(|v| v.as_u64()).unwrap_or(0);
        let stuck = if idle_secs >= 60 {
            format!(" [stuck {idle_secs}s]")
        } else {
            String::new()
        };
        match subphase {
            Some(sp) => lines.push(format!("→ {test_id}  ({sp}{stuck})")),
            None => lines.push(format!("→ {test_id}{stuck}")),
        }
    }

    // Queue drain ETA when we have backlog.
    if queued > 0 {
        if let Some(drain) = json
            .get("queue")
            .and_then(|q| q.get("estimated_drain_secs"))
            .and_then(|d| d.as_u64())
        {
            let mins = (drain + 59) / 60;
            lines.push(format!("Drain ETA: ~{mins} min"));
        }
    }

    // Browser idle countdown when Chrome is up but no test claims it.
    let browser_open = json
        .get("browser_open")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let browser_owner = json.get("browser_owner").and_then(|v| v.as_object());
    if browser_open && browser_owner.is_none() {
        if let Some(secs) = json
            .get("browser_idle_close_in_secs")
            .and_then(|v| v.as_u64())
        {
            lines.push(format!("Browser idle — auto-close in {secs}s"));
        }
    }

    Some(lines.join("\n"))
}

/// Open a URL in the user's default browser.  Currently Windows-only;
/// macOS/Linux fall through to a no-op (MVP scope).
fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use crate::platform::NoWindow;
        // `cmd /C start "" <url>` — the empty `""` arg is the window title
        // that `start` requires before its real argument when the URL
        // contains spaces.  Keep the shell hidden; only the browser should
        // appear.
        std::process::Command::new("cmd")
            .no_window()
            .args(["/C", "start", "", url])
            .spawn()
            .map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
}
