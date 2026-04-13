//! Left sidebar — agent list and navigation tabs.

use dioxus::prelude::*;

use super::View;
use crate::agent_config::AgentsFile;

#[component]
pub fn Sidebar(
    agents: Signal<AgentsFile>,
    pending_counts: Signal<std::collections::HashMap<String, usize>>,
    view: Signal<View>,
) -> Element {
    // Extract minimal data to avoid cloning entire AgentsFile/HashMap every render.
    let agents_list: Vec<(String, String, bool)> = agents.read().agents.iter()
        .map(|a| (a.id.clone(), a.identity.name.clone(), a.enabled))
        .collect();
    let counts = pending_counts.read().clone();
    let cur = view.read().clone();

    rsx! {
        div { class: "w-56 bg-gray-900 border-r border-gray-800 flex flex-col h-full",
            // Header
            div { class: "p-4 border-b border-gray-800",
                h1 { class: "text-lg font-bold text-white", "Sirin" }
                p { class: "text-xs text-gray-500 mt-1", "AI Agent Platform" }
            }

            // Agent list
            div { class: "flex-1 overflow-y-auto p-2 space-y-1",
                for (idx, (agent_id, name, enabled)) in agents_list.iter().enumerate() {
                    {
                        let is_selected = matches!(&cur, View::Workspace(i) if *i == idx);
                        let pending_n = counts.get(agent_id.as_str()).copied().unwrap_or(0);
                        let name = name.clone();
                        let agent_id = agent_id.clone();
                        let enabled = *enabled;
                        let bg_class = if is_selected { "bg-blue-900/50 border-blue-700" } else { "bg-gray-800/50 border-transparent hover:bg-gray-800" };
                        let enabled_class = if enabled { "text-green-400" } else { "text-gray-600" };
                        let enabled_label = if enabled { "● 啟用" } else { "○ 停用" };

                        rsx! {
                            div {
                                class: "p-3 rounded-lg border cursor-pointer transition-colors {bg_class}",
                                onclick: move |_| view.set(View::Workspace(idx)),
                                div { class: "flex items-center justify-between",
                                    span { class: "font-medium text-sm text-white truncate", "{name}" }
                                    if pending_n > 0 {
                                        span { class: "bg-orange-600 text-white text-xs px-1.5 py-0.5 rounded-full", "{pending_n}" }
                                    }
                                }
                                div { class: "flex items-center gap-2 mt-1",
                                    span { class: "text-xs {enabled_class}", "{enabled_label}" }
                                    span { class: "text-xs text-gray-500", "{agent_id}" }
                                }
                            }
                        }
                    }
                }
            }

            // Bottom navigation
            div { class: "p-2 border-t border-gray-800 space-y-1",
                {
                    let nav_items: Vec<(&str, View)> = vec![
                        ("⚙ 設定", View::Settings),
                        ("📋 Log", View::Log),
                        ("🔧 Workflow", View::Workflow),
                        ("🤝 Meeting", View::Meeting),
                    ];
                    rsx! {
                        for (label, target) in nav_items {
                            {
                                let is_active = std::mem::discriminant(&cur) == std::mem::discriminant(&target);
                                let bg = if is_active { "bg-gray-700" } else { "hover:bg-gray-800" };
                                let t = target.clone();
                                rsx! {
                                    div {
                                        class: "px-3 py-2 rounded-lg cursor-pointer text-sm transition-colors {bg}",
                                        onclick: move |_| view.set(t.clone()),
                                        "{label}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
