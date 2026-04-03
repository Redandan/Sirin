#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod persona;
mod telegram;
mod followup;
mod memory;
mod researcher;
mod skills;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WindowEvent,
};

fn task_log_path() -> std::path::PathBuf {
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

// ── Tauri commands ────────────────────────────────────────────────────────────

/// Return the last 50 task entries from the log file.
///
/// Called from the frontend via `invoke('read_tasks')`.
#[tauri::command]
fn read_tasks(
    state: tauri::State<'_, persona::TaskTracker>,
) -> Result<Vec<persona::TaskEntry>, String> {
    state
        .read_last_n(50)
        .map(|entries| {
            entries
                .into_iter()
                .filter(|entry| entry.event != "heartbeat")
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// Return the registered skills that can be dispatched by the backend.
#[tauri::command]
fn list_skills() -> Result<Vec<skills::SkillDefinition>, String> {
    Ok(skills::list_skills())
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
    let original_entry = state.find_by_timestamp(&timestamp).map_err(|e| e.to_string())?;

    // 1. Update the task status in the JSONL file.
    let mut updates = std::collections::HashMap::new();
    updates.insert(timestamp.clone(), "DONE".to_string());
    state.update_statuses(&updates).map_err(|e| e.to_string())?;

    // 2. Execute skill through dispatcher.
    let skill_name = skill.as_deref().unwrap_or("send_tg_reply");
    skills::execute_skill(&app, skill_name, &timestamp)?;

    // 3. Record the skill-execution event.
    let entry = persona::TaskEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        event: format!("skill_executed:{skill_name}"),
        persona: "Sirin".to_string(),
        message_preview: original_entry.and_then(|entry| entry.message_preview),
        trigger_remote_ai: None,
        estimated_profit_usd: None,
        status: Some("DONE".to_string()),
        reason: None,
        action_tier: None,
        high_priority: None,
    };
    state.record(&entry).map_err(|e| e.to_string())?;

    Ok(())
}

/// Search the web via DuckDuckGo without an API key.
///
/// Called from the frontend via `invoke('search_web', { query })`.
#[tauri::command]
async fn search_web(query: String) -> Result<Vec<skills::SearchResult>, String> {
    skills::ddg_search(&query).await
}

/// Return the most recent conversation context entries.
///
/// Called from the frontend via `invoke('get_context')`.
#[tauri::command]
fn get_context() -> Result<Vec<memory::ContextEntry>, String> {
    memory::load_recent_context(20).map_err(|e| e.to_string())
}

/// Wipe all stored conversation context.
///
/// Called from the frontend via `invoke('clear_context')`.
#[tauri::command]
fn clear_context() -> Result<(), String> {
    memory::clear_context().map_err(|e| e.to_string())
}

/// Start a background research task on a topic and optional URL.
///
/// Returns the new task ID immediately; the pipeline runs in the background.
/// Called from the frontend via `invoke('start_research', { topic, url })`.
#[tauri::command]
async fn start_research(topic: String, url: Option<String>) -> Result<String, String> {
    let task_id = format!("r-{}", chrono::Utc::now().timestamp_millis());
    let id_clone = task_id.clone();
    tokio::spawn(async move {
        let task = researcher::run_research(topic, url).await;
        eprintln!("[researcher] Task '{}' finished: {:?}", task.id, task.status);
    });
    Ok(id_clone)
}

/// Get the current status of a research task by ID.
///
/// Called from the frontend via `invoke('get_research_status', { id })`.
#[tauri::command]
fn get_research_status(id: String) -> Result<Option<researcher::ResearchTask>, String> {
    researcher::get_research(&id)
}

/// List all research tasks, newest first.
///
/// Called from the frontend via `invoke('list_research_tasks')`.
#[tauri::command]
fn list_research_tasks() -> Result<Vec<researcher::ResearchTask>, String> {
    let mut tasks = researcher::list_research()?;
    tasks.reverse();
    Ok(tasks)
}

// ── Persistent background loop (runs every 60 seconds) ───────────────────────

async fn background_loop() {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    let tracker = persona::TaskTracker::new(task_log_path());

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
        let entry = persona::TaskEntry::heartbeat(p.name());
        if let Err(e) = tracker.record(&entry) {
            eprintln!("[sirin] Failed to record task entry: {e}");
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // Load .env if present (non-fatal if missing)
    match dotenvy::dotenv() {
        Ok(path) => eprintln!("[main] Loaded .env from {:?}", path),
        Err(e) => eprintln!("[main] .env not loaded: {e}"),
    }

    // Shared task tracker exposed to Tauri commands.
    let tracker = persona::TaskTracker::new(task_log_path());

    tauri::Builder::default()
        .manage(tracker.clone())
        .invoke_handler(tauri::generate_handler![read_tasks, list_skills, approve_task, search_web, get_context, clear_context, start_research, get_research_status, list_research_tasks])
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
