//! Workspace view — agent overview, thinking stream, pending approvals.

use dioxus::prelude::*;

use crate::agent_config::AgentsFile;
use crate::pending_reply::{self, PendingReply, PendingStatus};
use crate::persona::TaskEntry;

#[derive(Clone, Copy, PartialEq)]
enum Tab { Overview, Pending }

#[component]
pub fn Workspace(
    agent_index: usize,
    agents: Signal<AgentsFile>,
    tasks: Signal<Vec<TaskEntry>>,
    pending_counts: Signal<std::collections::HashMap<String, usize>>,
) -> Element {
    let tab = use_signal(|| Tab::Overview);

    let agents_read = agents.read();
    let agent = match agents_read.agents.get(agent_index) {
        Some(a) => a,
        None => return rsx! { div { class: "p-8 text-gray-500", "Agent not found" } },
    };
    let agent_id = agent.id.clone();
    let agent_name = agent.identity.name.clone();
    let pending_n = pending_counts.read().get(&agent_id).copied().unwrap_or(0);
    let current_tab = *tab.read();

    rsx! {
        div { class: "flex flex-col h-full",
            // Agent header
            div { class: "p-4 border-b border-gray-800",
                h2 { class: "text-xl font-bold text-white", "{agent_name}" }
                p { class: "text-sm text-gray-500 mt-1", "ID: {agent_id}" }
            }

            // Tab bar
            div { class: "flex gap-1 p-2 border-b border-gray-800",
                TabButton { label: "📊 概覽", target: Tab::Overview, current: current_tab, tab: tab }
                TabButton { label: "📝 待確認", target: Tab::Pending, current: current_tab, tab: tab, badge: pending_n }
            }

            // Tab content
            div { class: "flex-1 overflow-y-auto p-4",
                match current_tab {
                    Tab::Overview => rsx! { OverviewContent { tasks: tasks } },
                    Tab::Pending => rsx! { PendingContent { agent_id: agent_id.clone() } },
                }
            }
        }
    }
}

// ── Tab button component ─────────────────────────────────────────────────────

#[component]
fn TabButton(
    label: String,
    target: Tab,
    current: Tab,
    tab: Signal<Tab>,
    #[props(default = 0)] badge: usize,
) -> Element {
    let is_active = current == target;
    let bg = if is_active { "bg-gray-700 text-white" } else { "text-gray-400 hover:bg-gray-800" };

    rsx! {
        button {
            class: "px-3 py-1.5 rounded-lg text-sm transition-colors flex items-center gap-1.5 {bg}",
            onclick: move |_| tab.set(target),
            "{label}"
            if badge > 0 {
                span { class: "bg-orange-600 text-white text-xs px-1.5 py-0.5 rounded-full", "{badge}" }
            }
        }
    }
}

// ── Overview tab ─────────────────────────────────────────────────────────────

#[component]
fn OverviewContent(tasks: Signal<Vec<TaskEntry>>) -> Element {
    let task_list = tasks.read();
    let recent: Vec<&TaskEntry> = task_list.iter().take(20).collect();

    rsx! {
        div { class: "space-y-4",
            h3 { class: "text-sm font-semibold text-gray-400 uppercase tracking-wider", "近期活動" }

            if recent.is_empty() {
                div { class: "text-gray-600 text-center py-8", "目前沒有活動記錄" }
            }

            for task in recent {
                div { class: "bg-gray-800/50 rounded-lg p-3 border border-gray-700/50",
                    div { class: "flex items-center justify-between",
                        span { class: "text-sm text-white", "{task.event}" }
                        {
                            let status = task.status.as_deref().unwrap_or("");
                            let color = status_color(status);
                            rsx! { span { class: "text-xs {color}", "{status}" } }
                        }
                    }
                    {
                        if let Some(ref reason) = task.reason {
                            rsx! { p { class: "text-xs text-gray-500 mt-1 truncate", "{reason}" } }
                        } else {
                            rsx! {}
                        }
                    }
                    span { class: "text-xs text-gray-600", "{task.timestamp}" }
                }
            }
        }
    }
}

// ── Pending approvals tab ────────────────────────────────────────────────────

#[component]
fn PendingContent(agent_id: String) -> Element {
    let mut replies: Signal<Vec<PendingReply>> = use_signal(|| Vec::new());

    // Load on mount
    use_effect({
        let id = agent_id.clone();
        move || { replies.set(pending_reply::load_pending(&id)); }
    });

    let pending: Vec<PendingReply> = replies.read()
        .iter()
        .filter(|r| r.status == PendingStatus::Pending)
        .cloned()
        .collect();
    let count = pending.len();

    rsx! {
        div { class: "space-y-4",
            h3 { class: "text-sm font-semibold text-gray-400 uppercase tracking-wider",
                "待確認回覆 ({count})"
            }

            if pending.is_empty() {
                div { class: "text-gray-600 text-center py-8", "✅ 沒有待確認的回覆" }
            }

            for reply in pending {
                PendingCard { reply: reply, agent_id: agent_id.clone(), replies: replies }
            }
        }
    }
}

#[component]
fn PendingCard(reply: PendingReply, agent_id: String, replies: Signal<Vec<PendingReply>>) -> Element {
    let id_approve = reply.id.clone();
    let id_reject = reply.id.clone();
    let aid_approve = agent_id.clone();
    let aid_reject = agent_id.clone();

    rsx! {
        div { class: "bg-gray-800/50 rounded-lg p-4 border border-gray-700/50 space-y-3",
            div { class: "flex items-center gap-2",
                span { class: "text-xs text-gray-500", "來自" }
                span { class: "text-sm font-medium text-blue-400", "{reply.peer_name}" }
                span { class: "text-xs text-gray-600 ml-auto", "{reply.created_at}" }
            }

            div { class: "bg-gray-900/50 rounded p-2",
                p { class: "text-sm text-gray-300", "{reply.original_message}" }
            }

            div { class: "bg-blue-900/20 rounded p-2 border border-blue-800/30",
                p { class: "text-sm text-blue-200", "{reply.draft_reply}" }
            }

            div { class: "flex gap-2",
                button {
                    class: "px-4 py-1.5 bg-green-700 hover:bg-green-600 text-white text-sm rounded-lg",
                    onclick: move |_| {
                        pending_reply::update_status(&aid_approve, &id_approve, PendingStatus::Approved);
                        replies.set(pending_reply::load_pending(&aid_approve));
                    },
                    "✓ 核准"
                }
                button {
                    class: "px-4 py-1.5 bg-red-800 hover:bg-red-700 text-white text-sm rounded-lg",
                    onclick: move |_| {
                        pending_reply::update_status(&aid_reject, &id_reject, PendingStatus::Rejected);
                        replies.set(pending_reply::load_pending(&aid_reject));
                    },
                    "✗ 拒絕"
                }
            }
        }
    }
}

fn status_color(status: &str) -> &'static str {
    match status {
        "DONE" => "text-green-400",
        "PENDING" | "RUNNING" => "text-yellow-400",
        "FOLLOWING" => "text-blue-400",
        "FOLLOWUP_NEEDED" => "text-orange-400",
        "FAILED" | "ERROR" => "text-red-400",
        _ => "text-gray-500",
    }
}
