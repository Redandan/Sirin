#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![recursion_limit = "256"]

mod adk;
#[allow(dead_code)] mod agent_config;
mod authz;
pub mod error;
mod platform;
#[allow(dead_code)] mod browser;
#[allow(dead_code)] mod browser_ax;
#[allow(dead_code)] mod claude_session;
#[allow(dead_code)] mod config_check;
mod diagnose;
#[allow(dead_code)] mod test_runner;
#[allow(dead_code)] mod agents;
mod code_graph;
mod events;
mod followup;
mod human_behavior;
mod jsonl_log;
mod log_subscriber;
#[allow(dead_code)] mod llm;
mod log_buffer;
#[allow(dead_code)] mod memory;
#[allow(dead_code)] mod pending_reply;
#[allow(dead_code)] mod persona;
#[allow(dead_code)] mod researcher;
#[allow(dead_code)] mod mcp_client;
mod mcp_server;
pub mod monitor;
mod rhai_engine;
mod rpc_server;
pub mod updater;
#[allow(dead_code)] mod teams;
#[allow(dead_code)] mod skill_loader;
mod skills;
#[allow(dead_code)] mod meeting;
#[allow(dead_code)] mod workflow;
mod telegram;
mod telegram_auth;
pub mod ui_service;
mod ui_service_impl;
mod ui_egui;

use std::path::PathBuf;
use std::sync::Arc as StdArc;

use persona::TaskTracker;
use telegram_auth::TelegramAuthState;

fn task_log_path() -> PathBuf {
    platform::app_data_dir().join("tracking").join("task.jsonl")
}

/// Ensure all required directories and default config files exist.
fn ensure_first_run_dirs() {
    use std::fs;

    let data = platform::app_data_dir();
    for sub in &["tracking", "memory", "code_graph", "context",
                 "pending_replies", "sessions", "teams_profile", "test_failures"] {
        let _ = fs::create_dir_all(data.join(sub));
    }

    let cfg = platform::config_dir();
    for sub in &["", "skills", "scripts", "tests"] {
        let _ = fs::create_dir_all(cfg.join(sub));
    }

    if !cfg.join("agents.yaml").exists() {
        let default_file = agent_config::AgentsFile::default();
        if let Ok(yaml) = serde_yaml::to_string(&default_file) {
            let _ = fs::write(cfg.join("agents.yaml"), yaml);
            eprintln!("[main] Created default config/agents.yaml");
        }
    }

    // .env: prefer app_data_dir, fall back to CWD (dev mode)
    let env_path = data.join(".env");
    if !env_path.exists() {
        let env_example = data.join(".env.example");
        if env_example.exists() && fs::copy(&env_example, &env_path).is_ok() {
            eprintln!("[main] Created .env from .env.example");
        }
    }

    if !cfg.join("persona.yaml").exists() {
        let default_yaml = "\
identity:\n  name: 助手1\n  professional_tone: brief\n\
response_style:\n  voice: 自然、親切\n  ack_prefix: 收到。\n  compliance_line: 我來協助你。\n\
objectives: []\nroi_thresholds:\n  min_usd_to_notify: 5.0\n  min_usd_to_call_remote_llm: 25.0\n\
coding_agent:\n  enabled: true\n  auto_approve_writes: true\n  max_iterations: 10\n";
        let _ = fs::write(cfg.join("persona.yaml"), default_yaml);
        eprintln!("[main] Created default config/persona.yaml");
    }
}

async fn background_loop(tracker: TaskTracker) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.tick().await;
    loop {
        interval.tick().await;
        if let Ok(p) = persona::Persona::cached() {
            let entry = persona::TaskEntry::heartbeat(p.name());
            let _ = tracker.record(&entry);
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    // Default filter: everything from sirin at info, everything else at warn —
    // keeps reqwest / hyper / tokio internals from flooding the UI.
    // Override with `RUST_LOG=sirin=debug,reqwest=info` etc.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("sirin=info,warn"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
        .with(log_subscriber::LogBufferLayer)
        .init();
}

fn main() {
    init_tracing();
    diagnose::record_startup();

    // Try app_data_dir/.env first (installed mode), then CWD (dev mode)
    let env_file = platform::app_data_dir().join(".env");
    if env_file.exists() {
        match dotenvy::from_path(&env_file) {
            Ok(_)  => eprintln!("[main] Loaded .env from {env_file:?}"),
            Err(e) => eprintln!("[main] .env load error: {e}"),
        }
    } else {
        match dotenvy::dotenv() {
            Ok(path) => eprintln!("[main] Loaded .env from {path:?}"),
            Err(e)   => eprintln!("[main] .env not loaded: {e}"),
        }
    }

    ensure_first_run_dirs();

    // ── AuthZ engine init ────────────────────────────────────────────────────
    authz::init(Some(std::path::Path::new(".")));
    eprintln!("[main] AuthZ engine initialized");

    // ── Live Monitor init ────────────────────────────────────────────────────
    {
        let _ = std::fs::create_dir_all(".sirin");
        monitor::init(monitor::MonitorConfig {
            trace_dir: std::path::PathBuf::from(".sirin"),
            trace_size_limit: None,
        });
        eprintln!("[main] Live Monitor initialized");
    }

    match memory::ensure_codebase_index() {
        Ok(count) if count > 0 => eprintln!("[main] Refreshed codebase index ({count} files)"),
        Ok(_) => {}
        Err(e) => eprintln!("[main] Codebase index refresh skipped: {e}"),
    }

    let tracker = TaskTracker::new(task_log_path());
    let tg_auth = TelegramAuthState::new();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    {
        let client = reqwest::Client::new();
        let fleet = rt.block_on(llm::probe_and_build_fleet(&client));
        fleet.log_summary();
        if fleet.chat_model.is_empty() {
            eprintln!("⚠️  [main] WARNING: No LLM models discovered.");
        }
        llm::init_shared_llm(fleet.to_llm_config());
        llm::init_agent_fleet(fleet);
    }

    // Run config diagnostics after fleet init — only prints if warnings/errors
    {
        let issues = config_check::run_diagnostics();
        config_check::log_startup(&issues);
    }

    {
        let tools = rt.block_on(mcp_client::init());
        if !tools.is_empty() {
            eprintln!("[main] MCP client: {} external tool(s) available", tools.len());
        }
    }

    let mut agent_auth_states: Vec<(String, TelegramAuthState)> = Vec::new();

    {
        let _guard = rt.enter();
        rt.spawn(background_loop(tracker.clone()));

        let agents = agent_config::AgentsFile::load().unwrap_or_default();
        let mut primary_auth_assigned = false;

        for agent in agents.agents.iter().filter(|a| a.enabled) {
            let Some(ch_cfg) = agent.channel.as_ref().and_then(|c| c.telegram.as_ref()) else {
                continue;
            };
            let agent_auth = if !primary_auth_assigned {
                primary_auth_assigned = true;
                tg_auth.clone()
            } else {
                TelegramAuthState::new()
            };
            agent_auth_states.push((agent.id.clone(), agent_auth.clone()));

            let tg_tracker = tracker.clone();
            let agent_cfg = agent.clone();
            let tg_channel = ch_cfg.clone();
            let auth_clone = agent_auth;
            rt.spawn(async move {
                telegram::run_agent_listener(agent_cfg, tg_channel, tg_tracker, auth_clone).await;
            });
        }

        if !primary_auth_assigned {
            eprintln!("[main] No Telegram channel in agents.yaml — falling back to env-var listener");
            let tg_tracker = tracker.clone();
            let tg_auth_spawn = tg_auth.clone();
            rt.spawn(async move {
                telegram::run_listener(tg_tracker, tg_auth_spawn).await;
            });
        }

        rt.spawn(followup::run_worker(tracker.clone()));
        rt.spawn(rpc_server::start_rpc_server());

        // Screenshot pump — starts only when Monitor view is active
        monitor::spawn_screenshot_pump();

        // Background update check — non-blocking, shows banner in UI if new version found
        updater::spawn_check();
    }

    std::mem::forget(rt);
    let svc = StdArc::new(ui_service_impl::RealService::new(tracker, tg_auth));
    ui_egui::launch(svc);
}
