//! Meeting room — multi-agent session management.

use dioxus::prelude::*;

use super::AppState;
use crate::agent_config::AgentsFile;

#[component]
pub fn MeetingRoom(agents: Signal<AgentsFile>) -> Element {
    let _app_state = use_context::<Signal<AppState>>();
    let mut input = use_signal(String::new);
    let mut messages: Signal<Vec<(String, String)>> = use_signal(Vec::new); // (speaker, text)
    let loading = use_signal(|| false);

    let agent_list = agents.read().agents.clone();

    rsx! {
        div { class: "flex flex-col h-full",
            // Header
            div { class: "p-4 border-b border-gray-800",
                h2 { class: "text-xl font-bold text-white", "🤝 多 Agent 會議室" }
                p { class: "text-sm text-gray-500 mt-1",
                    "參與者: {agent_list.iter().filter(|a| a.enabled).map(|a| a.identity.name.as_str()).collect::<Vec<_>>().join(\", \")}"
                }
            }

            // Messages
            div { class: "flex-1 overflow-y-auto p-4 space-y-3",
                if messages.read().is_empty() {
                    div { class: "text-gray-600 text-center py-12",
                        p { class: "text-4xl mb-3", "💬" }
                        p { "輸入訊息開始會議" }
                    }
                }
                for (speaker, text) in messages.read().iter() {
                    div { class: "bg-gray-800/50 rounded-lg p-3 border border-gray-700/50",
                        div { class: "text-xs font-semibold text-blue-400 mb-1", "{speaker}" }
                        div { class: "text-sm text-gray-200 whitespace-pre-wrap", "{text}" }
                    }
                }
                if *loading.read() {
                    div { class: "text-gray-500 text-sm animate-pulse", "⏳ AI 正在思考..." }
                }
            }

            // Input
            div { class: "p-4 border-t border-gray-800",
                div { class: "flex gap-2",
                    input {
                        class: "flex-1 bg-gray-900 border border-gray-700 rounded-lg px-3 py-2 text-white text-sm focus:border-blue-500 focus:outline-none",
                        placeholder: "輸入訊息...",
                        value: "{input}",
                        oninput: move |e| input.set(e.value()),
                        onkeypress: move |e| {
                            if e.key() == Key::Enter && !input.read().trim().is_empty() {
                                let msg = input.read().trim().to_string();
                                messages.write().push(("Operator".to_string(), msg));
                                input.set(String::new());
                            }
                        },
                    }
                    button {
                        class: "bg-blue-700 hover:bg-blue-600 text-white px-4 py-2 rounded-lg text-sm transition-colors",
                        disabled: input.read().trim().is_empty(),
                        onclick: move |_| {
                            let msg = input.read().trim().to_string();
                            if !msg.is_empty() {
                                messages.write().push(("Operator".to_string(), msg));
                                input.set(String::new());
                            }
                        },
                        "發送"
                    }
                }
            }
        }
    }
}
