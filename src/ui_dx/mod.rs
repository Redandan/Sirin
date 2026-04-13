//! Dioxus-based UI for Sirin.
//!
//! Cross-platform (Desktop / Web / Mobile) reactive UI built on Dioxus 0.7.
//! Background tasks (Telegram, follow-up worker, RPC) run in the Dioxus-managed
//! Tokio runtime via `spawn()`.

mod sidebar;
mod log_view;
mod workspace;
mod settings;
mod system;
mod workflow;
mod meeting;

use dioxus::prelude::*;

use crate::persona::{TaskEntry, TaskTracker};
use crate::telegram_auth::TelegramAuthState;

// ── Shared application state ─────────────────────────────────────────────────

/// Which top-level view is active.
#[derive(Clone, PartialEq, Debug)]
pub enum View {
    Workspace(usize),  // agent index
    Settings,
    Log,
    Workflow,
    Meeting,
}

/// Core application state shared via context.
#[derive(Clone)]
pub struct AppState {
    pub tracker: TaskTracker,
    pub tg_auth: TelegramAuthState,
    pub agent_auth_states: Vec<(String, TelegramAuthState)>,
}

// ── Static global for initial state (passed from main → Dioxus launch) ──────

static INIT_STATE: std::sync::OnceLock<AppState> = std::sync::OnceLock::new();

// ── Root component ───────────────────────────────────────────────────────────

#[component]
fn App() -> Element {
    // Inject shared state on first render.
    let init = INIT_STATE.get().expect("AppState not initialized");
    let app_state = use_context_provider(|| Signal::new(init.clone()));
    let view = use_signal(|| View::Workspace(0));
    let mut tasks: Signal<Vec<TaskEntry>> = use_signal(Vec::new);
    let mut pending_counts: Signal<std::collections::HashMap<String, usize>> = use_signal(std::collections::HashMap::new);
    let agents_file = use_signal(|| {
        crate::agent_config::AgentsFile::load().unwrap_or_default()
    });

    // ── Background: periodic refresh (every 5 seconds) ──────────────────────
    let tracker_clone = app_state.read().tracker.clone();
    let agents_for_refresh = agents_file.read().agents.clone();
    use_future(move || {
        let tracker = tracker_clone.clone();
        let agents = agents_for_refresh.clone();
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;

                // Load tasks
                if let Ok(entries) = tracker.read_last_n(200) {
                    let filtered: Vec<TaskEntry> = entries
                        .into_iter()
                        .filter(|e| e.event != "heartbeat")
                        .rev()
                        .collect();
                    tasks.set(filtered);
                }

                // Load pending counts
                let mut counts = std::collections::HashMap::new();
                for agent in &agents {
                    let count = crate::pending_reply::load_pending(&agent.id)
                        .into_iter()
                        .filter(|r| r.status == crate::pending_reply::PendingStatus::Pending)
                        .count();
                    counts.insert(agent.id.clone(), count);
                }
                pending_counts.set(counts);
            }
        }
    });

    // ── Layout ──────────────────────────────────────────────────────────────
    let current_view = view.read().clone();

    rsx! {
        div { class: "flex h-screen bg-gray-950 text-gray-100 font-sans",
            sidebar::Sidebar {
                agents: agents_file,
                pending_counts: pending_counts,
                view: view,
            }

            div { class: "flex-1 flex flex-col overflow-hidden",
                match current_view {
                    View::Log => rsx! { log_view::LogView {} },
                    View::Workspace(idx) => rsx! {
                        workspace::Workspace {
                            agent_index: idx,
                            agents: agents_file,
                            tasks: tasks,
                            pending_counts: pending_counts,
                        }
                    },
                    View::Settings => rsx! {
                        settings::Settings { agents: agents_file }
                    },
                    View::Workflow => rsx! {
                        workflow::WorkflowView {}
                    },
                    View::Meeting => rsx! {
                        meeting::MeetingRoom { agents: agents_file }
                    },
                }
            }
        }
    }
}

// ── App launcher ─────────────────────────────────────────────────────────────

pub fn launch(
    tracker: TaskTracker,
    tg_auth: TelegramAuthState,
    agent_auth_states: Vec<(String, TelegramAuthState)>,
) {
    let _ = INIT_STATE.set(AppState {
        tracker,
        tg_auth,
        agent_auth_states,
    });

    #[cfg(feature = "desktop")]
    {
        dioxus::LaunchBuilder::desktop()
            .with_cfg(
                dioxus::desktop::Config::new()
                    .with_window(
                        dioxus::desktop::WindowBuilder::new()
                            .with_title("Sirin")
                            .with_inner_size(dioxus::desktop::LogicalSize::new(1100.0, 740.0))
                            .with_min_inner_size(dioxus::desktop::LogicalSize::new(640.0, 480.0)),
                    )
            )
            .launch(App);
    }

    // Fullstack server mode: Dioxus serves SSR + WASM client
    #[cfg(all(feature = "server", not(feature = "desktop")))]
    {
        dioxus::launch(App);
    }
}
