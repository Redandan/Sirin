#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adk;
#[allow(dead_code)] mod agent_config;
pub mod error;
mod platform;
mod browser;
#[allow(dead_code)] mod agents;
mod code_graph;
mod events;
mod followup;
mod human_behavior;
#[allow(dead_code)] mod llm;
mod log_buffer;
#[allow(dead_code)] mod memory;
#[allow(dead_code)] mod pending_reply;
#[allow(dead_code)] mod persona;
#[allow(dead_code)] mod researcher;
#[allow(dead_code)] mod mcp_client;
mod mcp_server;
mod rhai_engine;
mod rpc_server;
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
use std::sync::Arc;

use persona::TaskTracker;
use telegram_auth::TelegramAuthState;

fn task_log_path() -> PathBuf {
    platform::app_data_dir().join("tracking").join("task.jsonl")
}

/// Ensure all required directories and default config files exist.
fn ensure_first_run_dirs() {
    use std::fs;

    let data = platform::app_data_dir();
    for sub in &["tracking", "memory", "code_graph", "context"] {
        let _ = fs::create_dir_all(data.join(sub));
    }

    for sub in &["data/pending_replies", "data/sessions", "data/teams_profile"] {
        let _ = fs::create_dir_all(sub);
    }
    let _ = fs::create_dir_all("data");
    let _ = fs::create_dir_all("config");
    let _ = fs::create_dir_all("config/skills");
    let _ = fs::create_dir_all("config/scripts");

    if !std::path::Path::new("config/agents.yaml").exists() {
        let default_file = agent_config::AgentsFile::default();
        if let Ok(yaml) = serde_yaml::to_string(&default_file) {
            let _ = fs::write("config/agents.yaml", yaml);
            eprintln!("[main] Created default config/agents.yaml");
        }
    }

    if !std::path::Path::new(".env").exists() {
        if std::path::Path::new(".env.example").exists() {
            if fs::copy(".env.example", ".env").is_ok() {
                eprintln!("[main] Created .env from .env.example");
            }
        }
    }

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
    interval.tick().await;
    loop {
        interval.tick().await;
        if let Ok(p) = persona::Persona::load() {
            let entry = persona::TaskEntry::heartbeat(p.name());
            let _ = tracker.record(&entry);
        }
    }
}

fn main() {
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
    }

    std::mem::forget(rt);
    ui_dx::launch(tracker, tg_auth, agent_auth_states);
}
