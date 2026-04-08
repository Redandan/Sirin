#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adk;
mod agent_config;
mod agents;
mod code_graph;
mod events;
mod followup;
mod human_behavior;
mod llm;
mod log_buffer;
mod memory;
mod pending_reply;
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

    match memory::ensure_codebase_index() {
        Ok(count) if count > 0 => eprintln!("[main] Refreshed codebase index ({count} files)"),
        Ok(_) => {}
        Err(e) => eprintln!("[main] Codebase index refresh skipped: {e}"),
    }

    // Shared state
    let tracker = TaskTracker::new(task_log_path());
    let tg_auth = TelegramAuthState::new();

    // Build a Tokio runtime that lives for the entire process lifetime.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    // ── Startup environment probe ─────────────────────────────────────────────
    // Query the configured LLM backend, classify all available models by their
    // capabilities, and build the AgentFleet before any agent work begins.
    // This must happen before the first call to `shared_llm()` or `shared_fleet()`.
    {
        let client = reqwest::Client::new();
        let fleet = rt.block_on(llm::probe_and_build_fleet(&client));
        fleet.log_summary();
        llm::init_shared_llm(fleet.to_llm_config());
        llm::init_agent_fleet(fleet);
    }

    // Per-agent auth states collected here and moved into SirinApp below.
    let mut agent_auth_states: Vec<(String, TelegramAuthState)> = Vec::new();

    // Spawn all background tasks onto the runtime.
    {
        let _guard = rt.enter();

        rt.spawn(background_loop(tracker.clone()));

        // ── Per-agent Telegram listeners ──────────────────────────────────────
        // Load agents.yaml and spawn an independent Tokio task for each enabled
        // agent that has a Telegram channel configured.  The first such agent
        // receives the primary `tg_auth` (shown in the Settings → System panel).
        // Additional agents get standalone auth states (no UI feedback yet).
        let agents = agent_config::AgentsFile::load().unwrap_or_default();
        let mut primary_auth_assigned = false;

        for agent in agents.agents.iter().filter(|a| a.enabled) {
            let Some(ch_cfg) = agent.channel.as_ref().and_then(|c| c.telegram.as_ref()) else {
                continue; // no Telegram channel — UI/test-only agent, nothing to spawn
            };

            let agent_auth = if !primary_auth_assigned {
                // Wire the first agent's auth to the UI status display.
                primary_auth_assigned = true;
                tg_auth.clone()
            } else {
                TelegramAuthState::new()
            };

            agent_auth_states.push((agent.id.clone(), agent_auth.clone()));

            let tg_tracker  = tracker.clone();
            let agent_cfg   = agent.clone();   // Phase 3a: pass full config
            let tg_channel  = ch_cfg.clone();
            let auth_clone  = agent_auth;
            rt.spawn(async move {
                telegram::run_agent_listener(agent_cfg, tg_channel, tg_tracker, auth_clone).await;
            });
        }

        // Fallback: if no agent in agents.yaml has a Telegram channel,
        // fall back to the legacy env-var driven listener so existing setups
        // keep working without needing to edit agents.yaml.
        if !primary_auth_assigned {
            eprintln!("[main] No Telegram channel configured in agents.yaml — falling back to env-var listener");
            let tg_tracker    = tracker.clone();
            let tg_auth_spawn = tg_auth.clone();
            rt.spawn(async move {
                telegram::run_listener(tg_tracker, tg_auth_spawn).await;
            });
        }

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
            Ok(Box::new(ui::SirinApp::new(tracker, tg_auth, rt_handle, agent_auth_states)))
        }),
    )
    .expect("Failed to run Sirin UI");
}
