#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod followup;
mod llm;
mod log_buffer;
mod memory;
mod persona;
mod researcher;
mod skills;
mod telegram;
mod telegram_auth;
mod ui;

use std::path::PathBuf;

use persona::TaskTracker;
use telegram_auth::TelegramAuthState;

fn task_log_path() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return std::path::Path::new(&local_app_data)
            .join("Sirin")
            .join("tracking")
            .join("task.jsonl");
    }
    std::path::Path::new("data")
        .join("tracking")
        .join("task.jsonl")
}

async fn background_loop(tracker: TaskTracker) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.tick().await; // skip the immediate first tick
    loop {
        interval.tick().await;
        if let Ok(p) = persona::Persona::load() {
            let entry = persona::TaskEntry::heartbeat(p.name());
            let _ = tracker.record(&entry);
        }
    }
}

fn main() {
    // Load .env (non-fatal if absent)
    match dotenvy::dotenv() {
        Ok(path) => eprintln!("[main] Loaded .env from {path:?}"),
        Err(e) => eprintln!("[main] .env not loaded: {e}"),
    }

    // Shared state
    let tracker = TaskTracker::new(task_log_path());
    let tg_auth = TelegramAuthState::new();

    // Build a Tokio runtime that lives for the entire process lifetime.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    // Spawn all background tasks onto the runtime.
    {
        let _guard = rt.enter();

        rt.spawn(background_loop(tracker.clone()));

        let tg_tracker = tracker.clone();
        let tg_auth_spawn = tg_auth.clone();
        rt.spawn(async move {
            telegram::run_listener(tg_tracker, tg_auth_spawn).await;
        });

        rt.spawn(followup::run_worker(tracker.clone()));
    }

    // Keep the runtime alive by storing it; drop order matters on Windows.
    let rt_handle = rt.handle().clone();
    std::mem::forget(rt); // runtime lives until process exit

    // Launch the native egui window on the main thread.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sirin")
            .with_inner_size([1100.0, 740.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Sirin",
        options,
        Box::new(move |cc| {
            ui::SirinApp::setup_fonts(&cc.egui_ctx);
            Ok(Box::new(ui::SirinApp::new(tracker, tg_auth, rt_handle)))
        }),
    )
    .expect("Failed to run Sirin UI");
}
