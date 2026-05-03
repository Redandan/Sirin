#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![recursion_limit = "512"]

mod adk;
#[allow(dead_code)] mod agent_config;
mod authz;
#[cfg(test)] mod devex;
pub mod error;
mod platform;
#[allow(dead_code)] mod browser;
#[allow(dead_code)] mod browser_ax;
#[allow(dead_code)] mod browser_exec;
#[allow(dead_code)] mod claude_session;
#[allow(dead_code)] mod multi_agent;
#[allow(dead_code)] mod config_check;
mod diagnose;
mod ext_server;
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
#[allow(dead_code)] mod kb_client;
#[allow(dead_code)] mod integrations;
#[allow(dead_code)] mod assistant;
#[allow(dead_code)] mod perception;
mod mcp_gateway;
mod mcp_server;
pub mod monitor;
mod process_group;
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
pub mod chat_history;
// (egui shell + ui_test_bus removed in Phase 7 — UI is now a web app
//  served by mcp_server at http://127.0.0.1:7700/ui/)

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
            tracing::info!(target: "sirin", "[main] Created default config/agents.yaml");
        }
    }

    // .env: prefer app_data_dir, fall back to CWD (dev mode)
    let env_path = data.join(".env");
    if !env_path.exists() {
        let env_example = data.join(".env.example");
        if env_example.exists() && fs::copy(&env_example, &env_path).is_ok() {
            tracing::info!(target: "sirin", "[main] Created .env from .env.example");
        }
    }

    // Dev-mode YAML sync (#123): if a repo `config/` directory exists next to
    // the binary (or CWD), copy any newer YAML files to LOCALAPPDATA/Sirin/config/
    // so tests always pick up the latest YAML without a manual cp step.
    sync_repo_config_to_appdata(&cfg);

    if !cfg.join("persona.yaml").exists() {
        let default_yaml = "\
identity:\n  name: 助手1\n  professional_tone: brief\n\
response_style:\n  voice: 自然、親切\n  ack_prefix: 收到。\n  compliance_line: 我來協助你。\n\
objectives: []\nroi_thresholds:\n  min_usd_to_notify: 5.0\n  min_usd_to_call_remote_llm: 25.0\n\
coding_agent:\n  enabled: true\n  auto_approve_writes: true\n  max_iterations: 10\n";
        let _ = fs::write(cfg.join("persona.yaml"), default_yaml);
        tracing::info!(target: "sirin", "[main] Created default config/persona.yaml");
    }
}

/// Sync YAML files from the repo `config/` directory to
/// `%LOCALAPPDATA%/Sirin/config/` in dev mode (closes #123).
///
/// Only copies files that are NEWER in the repo than in LOCALAPPDATA, so
/// manually customised LOCALAPPDATA files are not overwritten unless the
/// repo version changed.  Skips silently when no repo `config/` is found
/// (installed-binary mode).
fn sync_repo_config_to_appdata(appdata_config: &std::path::Path) {
    // Look for repo config/ relative to CWD (dev: `cargo run` from project root)
    // or next to the binary.
    let candidates = [
        std::path::PathBuf::from("config"),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("config")))
            .unwrap_or_default(),
    ];

    let repo_config = candidates.iter().find(|p| p.is_dir() && p != &appdata_config);
    let Some(repo_cfg) = repo_config else { return };

    let mut synced = 0usize;
    sync_dir_recursive(repo_cfg, appdata_config, &mut synced);
    if synced > 0 {
        tracing::info!(target: "sirin", "[config] synced {} YAML file(s) from repo → LOCALAPPDATA", synced);
    }
}

fn sync_dir_recursive(src: &std::path::Path, dst: &std::path::Path, count: &mut usize) {
    use std::fs;
    let Ok(entries) = fs::read_dir(src) else { return };
    let _ = fs::create_dir_all(dst);
    for entry in entries.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            sync_dir_recursive(&src_path, &dst_path, count);
        } else if src_path.extension().map(|e| e == "yaml" || e == "yml").unwrap_or(false) {
            // Only copy if src is newer than dst (or dst doesn't exist).
            let should_copy = {
                let dst_mtime = dst_path.metadata().and_then(|m| m.modified()).ok();
                let src_mtime = src_path.metadata().and_then(|m| m.modified()).ok();
                match (src_mtime, dst_mtime) {
                    (Some(s), Some(d)) => s > d,
                    _ => true, // dst missing or can't stat → copy
                }
            };
            if should_copy
                && fs::copy(&src_path, &dst_path).is_ok() {
                    *count += 1;
                }
        }
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

/// Detect whether we should run without auto-opening the browser to the
/// web UI. Triggered by either the `--headless` CLI flag or
/// `SIRIN_HEADLESS=1` env var. In headless mode Sirin still launches the
/// RPC/MCP server, browser singleton, Telegram listeners, etc. — only
/// the `open_browser()` call is skipped so the binary can run on a server
/// / over SSH / inside Docker. The web UI itself remains reachable at
/// `http://127.0.0.1:7700/ui/` if the user navigates there manually.
fn is_headless() -> bool {
    std::env::args().any(|a| a == "--headless")
        || std::env::var("SIRIN_HEADLESS").map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

fn main() {
    init_tracing();

    // Install the process-tree kill switch BEFORE any subprocess spawns.
    // On Windows this assigns sirin.exe to a Job Object with
    // KILL_ON_JOB_CLOSE — every child spawned afterwards (claude, node,
    // git, chrome, ...) is auto-terminated when sirin exits, even on
    // crash / taskkill / panic.  Prevents orphan claude.exe accumulation.
    process_group::install();

    diagnose::record_startup();

    // Try app_data_dir/.env first (installed mode), then CWD (dev mode)
    let env_file = platform::app_data_dir().join(".env");
    if env_file.exists() {
        match dotenvy::from_path(&env_file) {
            Ok(_)  => tracing::info!(target: "sirin", "[main] Loaded .env from {env_file:?}"),
            // .env load failure is meaningful — surface as warn so it lands in diagnose.recent_errors
            Err(e) => tracing::warn!(target: "sirin", "[main] .env load error: {e}"),
        }
    } else {
        match dotenvy::dotenv() {
            Ok(path) => tracing::info!(target: "sirin", "[main] Loaded .env from {path:?}"),
            // Common in fresh/installed mode — info is enough
            Err(e)   => tracing::info!(target: "sirin", "[main] .env not loaded: {e}"),
        }
    }

    ensure_first_run_dirs();

    // ── Privacy mask default (Issue #80) ─────────────────────────────────────
    // Read SIRIN_PRIVACY_MASK once .env is loaded.  Default = on (fail-secure).
    browser::init_privacy_mask_from_env();

    // ── AuthZ engine init ────────────────────────────────────────────────────
    authz::init(Some(std::path::Path::new(".")));
    tracing::info!(target: "sirin", "[main] AuthZ engine initialized");

    // ── Live Monitor init ────────────────────────────────────────────────────
    {
        let _ = std::fs::create_dir_all(".sirin");
        monitor::init(monitor::MonitorConfig {
            trace_dir: std::path::PathBuf::from(".sirin"),
            trace_size_limit: None,
        });
        tracing::info!(target: "sirin", "[main] Live Monitor initialized");
    }

    match memory::ensure_codebase_index() {
        Ok(count) if count > 0 => tracing::info!(target: "sirin", "[main] Refreshed codebase index ({count} files)"),
        Ok(_) => {}
        // Index failure should surface — code_graph features quietly degrade otherwise
        Err(e) => tracing::warn!(target: "sirin", "[main] Codebase index refresh skipped: {e}"),
    }

    let tracker = TaskTracker::new(task_log_path());
    let tg_auth = TelegramAuthState::new();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    {
        let client = reqwest::Client::new();
        let fleet = rt.block_on(llm::probe_and_build_fleet(&client));
        fleet.log_summary();
        if fleet.chat_model.is_empty() {
            // No models = AI is dead — must surface in diagnose.recent_errors
            tracing::warn!(target: "sirin", "[main] No LLM models discovered — AI features will be unavailable");
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
            tracing::info!(target: "sirin", "[main] MCP client: {} external tool(s) available", tools.len());
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
            tracing::info!(target: "sirin", "[main] No Telegram channel in agents.yaml — falling back to env-var listener");
            let tg_tracker = tracker.clone();
            let tg_auth_spawn = tg_auth.clone();
            rt.spawn(async move {
                telegram::run_listener(tg_tracker, tg_auth_spawn).await;
            });
        }

        rt.spawn(followup::run_worker(tracker.clone()));
        rt.spawn(rpc_server::start_rpc_server());
        // Background LLM reachability probe — feeds diagnose.llm.reachable.
        // Cheap (one HTTP GET every 30s); ensures stale "model configured but
        // Ollama is down" cases get reported truthfully via the MCP tool.
        diagnose::spawn_reachability_probe();

        // Screenshot pump — starts only when Monitor view is active
        monitor::spawn_screenshot_pump();

        // Background update check — non-blocking, shows banner in UI if new version found
        updater::spawn_check();
    }

    std::mem::forget(rt);

    if is_headless() {
        tracing::info!(target: "sirin",
            "[main] Headless mode — RPC/MCP server on :{}, no GUI. Press Ctrl-C to exit.",
            std::env::var("SIRIN_RPC_PORT").unwrap_or_else(|_| "7700".into())
        );
        // Drop the unused service builder so we don't allocate UI state.
        drop((tracker, tg_auth));
        // Park forever — background threads (RPC server, telegram listener,
        // followup worker, screenshot pump) keep working.  Ctrl-C / taskkill
        // brings the process down cleanly.
        loop {
            std::thread::park();
        }
    }

    let svc = StdArc::new(ui_service_impl::RealService::new(tracker, tg_auth));
    // Web UI (`/ui/*` + `/api/snapshot`) lives at port 7700 in the same
    // process as the MCP server. Register the service so /api/snapshot can
    // read agents / runs / coverage.
    mcp_server::register_app_service(svc.clone() as StdArc<dyn ui_service::AppService>);

    // Auto-open the user's default browser to the UI page. Best-effort —
    // failure (no display, no browser registered) is fine; the user can
    // open the URL themselves.
    let port = std::env::var("SIRIN_RPC_PORT").unwrap_or_else(|_| "7700".into());
    let url = format!("http://127.0.0.1:{port}/ui/");
    open_browser(&url);
    tracing::info!(target: "sirin",
        "[main] Web UI: {url}  —  daemon will keep running with the browser tab closed.");

    // Park the main thread forever — RPC/MCP server, telegram listener,
    // followup worker, screenshot pump, etc. all keep working.  Closing the
    // browser tab no longer shuts the daemon down (this is the killer
    // feature of the daemon-style UX over the old egui window).
    loop {
        std::thread::park();
    }
}

/// Open `url` in the user's default browser. Cross-platform best-effort.
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use crate::platform::NoWindow;
        let _ = std::process::Command::new("cmd")
            .no_window()
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}
