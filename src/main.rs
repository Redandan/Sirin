#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adk;
mod agent_config;
mod log_manager;
mod platform;
mod browser;
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
mod mcp_server;
mod rpc_server;
mod teams;
mod skill_loader;
mod skills;
mod workflow;
mod telegram;
mod telegram_auth;
mod ui;

use std::path::PathBuf;

use persona::TaskTracker;
use telegram_auth::TelegramAuthState;

fn task_log_path() -> PathBuf {
    platform::app_data_dir().join("tracking").join("task.jsonl")
}

/// Ensure all required directories and default config files exist.
/// Called once at startup — safe to call repeatedly (all operations are idempotent).
fn ensure_first_run_dirs() {
    use std::fs;

    // App-data subdirectories
    let data = platform::app_data_dir();
    for sub in &["tracking", "memory", "code_graph", "context"] {
        let _ = fs::create_dir_all(data.join(sub));
    }

    // Local data directories (pending replies, sessions, teams profile, workflow)
    for sub in &["data/pending_replies", "data/sessions", "data/teams_profile"] {
        let _ = fs::create_dir_all(sub);
    }
    let _ = fs::create_dir_all("data");

    // config/ directory
    let _ = fs::create_dir_all("config");
    let _ = fs::create_dir_all("config/skills");
    let _ = fs::create_dir_all("config/scripts");

    // Write default agents.yaml if absent
    if !std::path::Path::new("config/agents.yaml").exists() {
        let default_file = agent_config::AgentsFile::default();
        if let Ok(yaml) = serde_yaml::to_string(&default_file) {
            let _ = fs::write("config/agents.yaml", yaml);
            eprintln!("[main] Created default config/agents.yaml");
        }
    }

    // Write default persona.yaml if absent
    if !std::path::Path::new("config/persona.yaml").exists() {
        let default_yaml = "\
identity:\n  name: 助手1\n  professional_tone: brief\n\
response_style:\n  voice: 自然、親切\n  ack_prefix: 收到。\n  compliance_line: 我來協助你。\n\
objectives: []\nroi_thresholds:\n  min_usd_to_notify: 5.0\n  min_usd_to_call_remote_llm: 25.0\n\
coding_agent:\n  enabled: true\n  auto_approve_writes: true\n  max_iterations: 10\n";
        let _ = fs::write("config/persona.yaml", default_yaml);
        eprintln!("[main] Created default config/persona.yaml");
    }
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

    ensure_first_run_dirs();

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

        // ── Local WebSocket RPC server ─────────────────────────────────────────
        rt.spawn(rpc_server::start_rpc_server());

        // Teams browser poller is started on demand from the Settings UI.
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
