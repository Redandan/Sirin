#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod persona;
mod telegram;
mod telegram_auth;
mod followup;
mod memory;
mod researcher;
mod skills;
mod logs;
mod optimization;

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

/// Return paginated task entries.
///
/// Called from the frontend via `invoke('read_tasks_paginated', { offset, limit })`.
#[tauri::command]
fn read_tasks_paginated(
    offset: usize,
    limit: usize,
    state: tauri::State<'_, persona::TaskTracker>,
) -> Result<serde_json::Value, String> {
    let limit = limit.min(100); // Cap at 100 per request
    let all_entries = state.read_last_n(1000).map_err(|e| e.to_string())?;
    
    let filtered: Vec<_> = all_entries
        .into_iter()
        .filter(|entry| entry.event != "heartbeat")
        .collect();
    
    let total = filtered.len();
    let start = offset.min(total);
    let end = (offset + limit).min(total);
    
    let items: Vec<_> = filtered
        .into_iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    
    Ok(serde_json::json!({
        "items": items,
        "total": total,
        "offset": offset,
        "limit": limit,
        "has_more": end < total,
    }))
}

/// Return system logs (structured logging entries).
///
/// Called from the frontend via `invoke('get_logs', { limit, offset, target, level })`.
#[tauri::command]
fn get_logs(
    limit: usize,
    offset: usize,
    target: Option<String>,
    level: Option<String>,
    state: tauri::State<'_, logs::LogStore>,
) -> Result<serde_json::Value, String> {
    let limit = limit.min(200); // Cap at 200 per request
    let target_opt = target.as_deref();
    let level_opt = level.as_deref();
    
    let filtered = state.filter(target_opt, level_opt);
    let total = filtered.len();
    let start = offset.min(total);
    let end = (offset + limit).min(total);
    
    let items: Vec<_> = filtered
        .into_iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    
    Ok(serde_json::json!({
        "items": items,
        "total": total,
        "offset": offset,
        "limit": limit,
        "has_more": end < total,
    }))
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

/// Record one model interaction for local optimization flywheel.
///
/// Called from the frontend via `invoke('record_interaction', { ... })`.
#[tauri::command]
fn record_interaction(
    source: String,
    input: String,
    output: String,
    latency_ms: u64,
    success: bool,
    model: Option<String>,
    prompt_version: Option<String>,
    metadata: Option<serde_json::Value>,
) -> Result<String, String> {
    optimization::record_interaction(
        source,
        input,
        output,
        latency_ms,
        success,
        model,
        prompt_version,
        metadata,
    )
}

/// Record user feedback for a previous interaction.
///
/// Called from the frontend via `invoke('record_feedback', { ... })`.
#[tauri::command]
fn record_feedback(
    interaction_id: String,
    rating: i8,
    reason: Option<String>,
    corrected_output: Option<String>,
    state: tauri::State<'_, persona::TaskTracker>,
) -> Result<String, String> {
    let feedback_id = optimization::record_feedback(
        interaction_id.clone(),
        rating,
        reason.clone(),
        corrected_output.clone(),
    )?;

    // Negative feedback becomes a self-improvement task that the autonomous loop can pick up.
    if rating < 0 {
        let interaction = optimization::get_interaction(&interaction_id)?;

        let msg = if let Some(c) = corrected_output.as_ref().filter(|v| !v.trim().is_empty()) {
            format!("自我優化任務: 針對回覆錯誤進行修正與調研。使用者修正版本: {c}")
        } else if let Some(r) = reason.as_ref().filter(|v| !v.trim().is_empty()) {
            format!("自我優化任務: 使用者負回饋原因: {r}")
        } else if let Some(i) = interaction {
            format!(
                "自我優化任務: 重新研究並改進以下對話。input: {} output: {}",
                i.input, i.output
            )
        } else {
            "自我優化任務: 收到負回饋，請重新調研並改進輸出品質。".to_string()
        };

        let entry = persona::TaskEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            event: "self_improvement_request".to_string(),
            persona: "Sirin".to_string(),
            message_preview: Some(msg),
            trigger_remote_ai: None,
            estimated_profit_usd: Some(1.0),
            status: Some("PENDING".to_string()),
            reason: Some(format!("feedback_id={feedback_id}")),
            action_tier: None,
            high_priority: Some(true),
        };

        state.record(&entry).map_err(|e| e.to_string())?;
    }

    Ok(feedback_id)
}

/// Return latest feedback entries for analysis dashboard.
///
/// Called from the frontend via `invoke('read_recent_feedback', { limit })`.
#[tauri::command]
fn read_recent_feedback(limit: usize) -> Result<Vec<optimization::FeedbackRecord>, String> {
    optimization::read_recent_feedback(limit.min(200))
}

/// Return current autonomous worker metrics for monitoring dashboard.
///
/// Called from the frontend via `invoke('read_autonomous_metrics')`.
#[tauri::command]
fn read_autonomous_metrics() -> Result<optimization::AutonomousMetrics, String> {
    optimization::read_autonomous_metrics()
}

/// Return the current Telegram auth/connection status.
///
/// Called from the frontend via `invoke('telegram_get_auth_status')`.
#[tauri::command]
fn telegram_get_auth_status(
    state: tauri::State<'_, telegram_auth::TelegramAuthState>,
) -> telegram_auth::TelegramStatus {
    state.status()
}

/// Feed a login code entered by the user into the waiting Telegram sign-in flow.
///
/// Returns `true` when the code was accepted (a flow was waiting), `false`
/// when no sign-in was pending.
///
/// Called from the frontend via `invoke('telegram_submit_auth_code', { code })`.
#[tauri::command]
fn telegram_submit_auth_code(
    code: String,
    state: tauri::State<'_, telegram_auth::TelegramAuthState>,
) -> bool {
    state.submit_code(code)
}

/// Feed a 2-FA password entered by the user into the waiting Telegram sign-in flow.
///
/// Called from the frontend via `invoke('telegram_submit_auth_password', { password })`.
#[tauri::command]
fn telegram_submit_auth_password(
    password: String,
    state: tauri::State<'_, telegram_auth::TelegramAuthState>,
) -> bool {
    state.submit_password(password)
}

async fn background_loop(tracker: persona::TaskTracker) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

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
    
    // Shared log store for structured logging.
    let log_store = logs::LogStore::new();

    // Shared Telegram auth state (non-blocking UI-driven login flow).
    let tg_auth = telegram_auth::TelegramAuthState::new();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .manage(tracker.clone())
        .manage(log_store.clone())
        .manage(tg_auth.clone())
        .invoke_handler(tauri::generate_handler![
            read_tasks,
            read_tasks_paginated,
            get_logs,
            list_skills,
            approve_task,
            search_web,
            get_context,
            clear_context,
            start_research,
            get_research_status,
            list_research_tasks,
            record_interaction,
            record_feedback,
            read_recent_feedback,
            read_autonomous_metrics,
            telegram_get_auth_status,
            telegram_submit_auth_code,
            telegram_submit_auth_password
        ])
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

            // Spawn the persistent background loop.
            // Reuse the shared tracker so all task writes share the same mutex.
            let bg_tracker = tracker.clone();
            tauri::async_runtime::spawn(background_loop(bg_tracker));

            // Spawn the Telegram listener (non-blocking; retries automatically)
            let tg_tracker = tracker.clone();
            let tg_auth_spawn = tg_auth.clone();
            tauri::async_runtime::spawn(async move {
                telegram::run_listener(tg_tracker, tg_auth_spawn).await;
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
