//! Settings page — per-agent configuration + system panel.

use dioxus::prelude::*;

use crate::agent_config::{AgentConfig, AgentsFile};

#[derive(Clone, Copy, PartialEq)]
enum Selection { Agent(usize), System }

#[component]
pub fn Settings(agents: Signal<AgentsFile>) -> Element {
    let mut selected = use_signal(|| Selection::Agent(0));
    let agents_read = agents.read();
    let agent_list = &agents_read.agents;
    let sel = *selected.read();

    rsx! {
        div { class: "flex h-full",
            // Left selector
            div { class: "w-48 border-r border-gray-800 flex flex-col h-full",
                div { class: "flex-1 p-3 space-y-1 overflow-y-auto",
                    h3 { class: "text-sm font-semibold text-gray-400 uppercase mb-2", "Agent 列表" }
                    for (idx, agent) in agent_list.iter().enumerate() {
                        {
                            let is_active = sel == Selection::Agent(idx);
                            let bg = if is_active { "bg-gray-700 text-white" } else { "text-gray-400 hover:bg-gray-800" };
                            let name = agent.identity.name.clone();
                            rsx! {
                                div {
                                    class: "px-3 py-2 rounded cursor-pointer text-sm {bg}",
                                    onclick: move |_| selected.set(Selection::Agent(idx)),
                                    "{name}"
                                }
                            }
                        }
                    }
                }
                // System button at bottom
                div { class: "p-3 border-t border-gray-800",
                    {
                        let is_sys = sel == Selection::System;
                        let bg = if is_sys { "bg-gray-700 text-white" } else { "text-gray-400 hover:bg-gray-800" };
                        rsx! {
                            div {
                                class: "px-3 py-2 rounded cursor-pointer text-sm {bg}",
                                onclick: move |_| selected.set(Selection::System),
                                "⚙ 系統"
                            }
                        }
                    }
                }
            }

            // Right content
            div { class: "flex-1 overflow-y-auto p-6",
                match sel {
                    Selection::Agent(idx) => {
                        if let Some(agent) = agent_list.get(idx) {
                            rsx! { AgentDetail { agent: agent.clone() } }
                        } else {
                            rsx! { div { class: "text-gray-500", "選擇一個 Agent" } }
                        }
                    }
                    Selection::System => rsx! {
                        super::system::SystemPanel {}
                    },
                }
            }
        }
    }
}

#[component]
fn AgentDetail(agent: AgentConfig) -> Element {
    let channel_info = if agent.channel.as_ref().and_then(|c| c.telegram.as_ref()).is_some() {
        "📱 Telegram"
    } else if agent.channel.as_ref().and_then(|c| c.teams.as_ref()).is_some() {
        "💼 Teams"
    } else {
        "🖥 UI Only"
    };

    rsx! {
        div { class: "space-y-6",
            // Header
            div { class: "flex items-center gap-4",
                h2 { class: "text-2xl font-bold text-white", "{agent.identity.name}" }
                {
                    let (cls, lbl) = if agent.enabled {
                        ("bg-green-900/50 text-green-400", "啟用")
                    } else {
                        ("bg-gray-800 text-gray-500", "停用")
                    };
                    rsx! { span { class: "text-sm px-2 py-0.5 rounded {cls}", "{lbl}" } }
                }
            }

            // Info cards
            div { class: "grid grid-cols-2 gap-4",
                InfoCard { label: "ID".to_string(), value: agent.id.clone() }
                InfoCard { label: "通道".to_string(), value: channel_info.to_string() }
                InfoCard { label: "語氣".to_string(), value: format!("{:?}", agent.identity.professional_tone) }
                InfoCard { label: "遠端 AI".to_string(), value: if agent.disable_remote_ai { "已禁用".to_string() } else { "允許".to_string() } }
            }

            // Objectives
            div { class: "bg-gray-800/50 rounded-lg p-4 border border-gray-700/50",
                h3 { class: "text-sm font-semibold text-gray-400 uppercase mb-3", "目標 Objectives" }
                if agent.objectives.is_empty() {
                    p { class: "text-gray-600 text-sm", "（使用全域 Persona 目標）" }
                }
                for obj in &agent.objectives {
                    div { class: "flex items-center gap-2 py-1",
                        span { class: "text-blue-400", "•" }
                        span { class: "text-sm text-gray-300", "{obj}" }
                    }
                }
            }

            // Human behavior
            div { class: "bg-gray-800/50 rounded-lg p-4 border border-gray-700/50",
                h3 { class: "text-sm font-semibold text-gray-400 uppercase mb-3", "人性化行為" }
                div { class: "grid grid-cols-2 gap-3 text-sm",
                    InfoRow { label: "啟用".to_string(), value: format!("{}", agent.human_behavior.enabled) }
                    InfoRow { label: "延遲範圍".to_string(), value: format!("{}–{}s", agent.human_behavior.min_reply_delay_secs, agent.human_behavior.max_reply_delay_secs) }
                    InfoRow { label: "每小時上限".to_string(), value: format!("{}", agent.human_behavior.max_messages_per_hour) }
                    InfoRow { label: "每日上限".to_string(), value: format!("{}", agent.human_behavior.max_messages_per_day) }
                }
            }

            // KPI
            if !agent.kpi.metrics.is_empty() {
                div { class: "bg-gray-800/50 rounded-lg p-4 border border-gray-700/50",
                    h3 { class: "text-sm font-semibold text-gray-400 uppercase mb-3", "KPI 指標" }
                    for metric in &agent.kpi.metrics {
                        div { class: "flex justify-between py-1",
                            span { class: "text-sm text-gray-300", "{metric.label}" }
                            span { class: "text-sm text-gray-500", "{metric.unit}" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn InfoCard(label: String, value: String) -> Element {
    rsx! {
        div { class: "bg-gray-800/50 rounded-lg p-3 border border-gray-700/50",
            p { class: "text-xs text-gray-500 uppercase", "{label}" }
            p { class: "text-sm text-white mt-1", "{value}" }
        }
    }
}

#[component]
fn InfoRow(label: String, value: String) -> Element {
    rsx! {
        div { class: "flex justify-between",
            span { class: "text-gray-500", "{label}" }
            span { class: "text-gray-300", "{value}" }
        }
    }
}
