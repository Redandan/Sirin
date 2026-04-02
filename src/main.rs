#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod persona;
mod telegram;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WindowEvent,
};

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

    tauri::Builder::default()
        .setup(|app| {
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
            tokio::spawn(background_loop());

            // Spawn the Telegram listener (non-fatal if credentials are absent)
            let tg_tracker = persona::TaskTracker::new("data/tracking/task.jsonl");
            tokio::spawn(async move {
                if let Err(e) = telegram::run_listener(tg_tracker).await {
                    eprintln!("[telegram] Listener exited: {e}");
                }
            });

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

