#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WindowEvent,
};

// ── Persona config loaded from config/persona.yaml ───────────────────────────

#[derive(Debug, Deserialize)]
struct Persona {
    name: String,
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    description: String,
}

// ── JSONL log entry written to data/tracking/task.jsonl ──────────────────────

#[derive(Debug, Serialize)]
struct TaskEntry {
    timestamp: String,
    event: String,
    persona: String,
}

// ── Persistent background loop (runs every 60 seconds) ───────────────────────

async fn background_loop() {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    let tracking_dir = PathBuf::from("data/tracking");

    // Consume the first instant tick so the loop waits a full 60 seconds before its first execution
    interval.tick().await;

    loop {
        interval.tick().await;

        // Read and parse config/persona.yaml
        let persona: Persona = match fs::read_to_string("config/persona.yaml") {
            Ok(content) => match serde_yaml::from_str(&content) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[sirin] Failed to parse config/persona.yaml: {e}");
                    continue;
                }
            },
            Err(e) => {
                eprintln!("[sirin] Failed to read config/persona.yaml: {e}");
                continue;
            }
        };

        // Ensure tracking directory exists
        if let Err(e) = fs::create_dir_all(&tracking_dir) {
            eprintln!("[sirin] Failed to create tracking dir: {e}");
            continue;
        }

        // Append a heartbeat entry to data/tracking/task.jsonl
        let entry = TaskEntry {
            timestamp: Utc::now().to_rfc3339(),
            event: "heartbeat".to_string(),
            persona: persona.name.clone(),
        };

        match serde_json::to_string(&entry) {
            Ok(line) => {
                match OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(tracking_dir.join("task.jsonl"))
                {
                    Ok(mut file) => {
                        if let Err(e) = writeln!(file, "{line}") {
                            eprintln!("[sirin] Failed to write task entry: {e}");
                        }
                    }
                    Err(e) => eprintln!("[sirin] Failed to open task.jsonl: {e}"),
                }
            }
            Err(e) => eprintln!("[sirin] Failed to serialize task entry: {e}"),
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
