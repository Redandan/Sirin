#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod persona;
mod telegram;
mod followup;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, WindowEvent,
};

// ── Tauri commands ────────────────────────────────────────────────────────────

/// Return the last 50 task entries from the log file.
///
/// Called from the frontend via `invoke('read_tasks')`.
#[tauri::command]
fn read_tasks(
    state: tauri::State<'_, persona::TaskTracker>,
) -> Result<Vec<persona::TaskEntry>, String> {
    state.read_last_n(50).map_err(|e| e.to_string())
}

/// Mark a task as approved (status → `"DONE"`) and record a `skill_executed`
/// log entry.
///
/// The `skill` parameter names the predefined action to invoke (e.g.
/// `"send_tg_reply"`).  Emits a `"skill:<skill>"` Tauri event so that
/// background modules (e.g. the Telegram listener) can react.
///
/// Called from the frontend via `invoke('approve_task', { timestamp, skill })`.
#[tauri::command]
fn approve_task(
    timestamp: String,
    skill: Option<String>,
    app: tauri::AppHandle,
    state: tauri::State<'_, persona::TaskTracker>,
) -> Result<(), String> {
    // 1. Update the task status in the JSONL file.
    let mut updates = std::collections::HashMap::new();
    updates.insert(timestamp.clone(), "DONE".to_string());
    state.update_statuses(&updates).map_err(|e| e.to_string())?;

    // 2. Record the skill-execution event.
    let skill_name = skill.as_deref().unwrap_or("send_tg_reply");
    let entry = persona::TaskEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        event: format!("skill_executed:{skill_name}"),
        persona: "Sirin".to_string(),
        trigger_remote_ai: None,
        estimated_profit_usd: None,
        status: Some("DONE".to_string()),
    };
    state.record(&entry).map_err(|e| e.to_string())?;

    // 3. Emit an event so background modules can react (e.g. TG sender).
    let _ = app.emit(&format!("skill:{skill_name}"), &timestamp);

    Ok(())
}

// ── Persistent background loop (runs every 60 seconds) ───────────────────────

async fn background_loop() {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    let tracker = persona::TaskTracker::new("data/tracking/task.jsonl");

    // Consume the first instant tick so the loop waits a full 60 seconds before its first execution
    interval.tick().await;

    loop {
        interval.tick().await;

        // Read and parse config/persona.yaml
        let p = match persona::Persona::load() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[sirin] Failed to load persona: {e}");
                continue;
            }
        };

        // Log the periodic heartbeat
        let entry = persona::TaskEntry::heartbeat(&p.name);
        if let Err(e) = tracker.record(&entry) {
            eprintln!("[sirin] Failed to record task entry: {e}");
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // Load .env if present (non-fatal if missing)
    let _ = dotenvy::dotenv();

    // Shared task tracker exposed to Tauri commands.
    let tracker = persona::TaskTracker::new("data/tracking/task.jsonl");

    tauri::Builder::default()
        .manage(tracker.clone())
        .invoke_handler(tauri::generate_handler![read_tasks, approve_task])
        .setup(move |app| {
            // Build tray menu items
            let show_item =
                MenuItem::with_id(app, "show", "Show Sirin", true, None::<&str>)?;
            let quit_item =
                MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            // Register system tray icon with the menu
            TrayIconBuilder::new()
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            // Spawn the persistent background loop
            tauri::async_runtime::spawn(background_loop());

            // Spawn the Telegram listener (non-fatal if credentials are absent)
            let tg_tracker = tracker.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = telegram::run_listener(tg_tracker).await {
                    eprintln!("[telegram] Listener exited: {e}");
                }
            });

            // Spawn the follow-up worker (runs every 30 minutes)
            let fu_tracker = tracker.clone();
            tauri::async_runtime::spawn(followup::run_worker(fu_tracker));

            Ok(())
        })
        // Hide the window on close instead of quitting (background-first behavior)
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Sirin");
}
