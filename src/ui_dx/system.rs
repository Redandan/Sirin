//! System panel — LLM config, Telegram auth status, data management.

use dioxus::prelude::*;

use super::AppState;

#[component]
pub fn SystemPanel() -> Element {
    let app_state = use_context::<Signal<AppState>>();
    let tg_auth = app_state.read().tg_auth.clone();
    let tg_status_enum = tg_auth.status();
    let is_connected = matches!(tg_status_enum, crate::telegram_auth::TelegramStatus::Connected);
    let tg_status = format!("{:?}", tg_status_enum);

    let rpc_running = crate::rpc_server::is_running();

    rsx! {
        div { class: "p-6 space-y-6 overflow-y-auto h-full",
            h2 { class: "text-xl font-bold text-white", "系統設定" }

            // Connection status
            div { class: "bg-gray-800/50 rounded-lg p-4 border border-gray-700/50",
                h3 { class: "text-sm font-semibold text-gray-400 uppercase mb-3", "連線狀態" }
                div { class: "space-y-2",
                    StatusRow {
                        label: "Telegram".to_string(),
                        status: tg_status,
                        ok: is_connected,
                    }
                    StatusRow {
                        label: "RPC/MCP Server".to_string(),
                        status: if rpc_running { "Running (port 7700)".to_string() } else { "Stopped".to_string() },
                        ok: rpc_running,
                    }
                }
            }

            // LLM Config
            div { class: "bg-gray-800/50 rounded-lg p-4 border border-gray-700/50",
                h3 { class: "text-sm font-semibold text-gray-400 uppercase mb-3", "LLM 配置" }
                {
                    let llm = crate::llm::shared_llm();
                    let router = crate::llm::shared_router_llm();
                    rsx! {
                        div { class: "space-y-2 text-sm",
                            InfoRow { label: "主模型".to_string(), value: format!("{} ({})", llm.model, llm.backend_name()) }
                            InfoRow { label: "Router 模型".to_string(), value: format!("{} ({})", router.model, router.backend_name()) }
                            InfoRow { label: "遠端".to_string(), value: if llm.is_remote() { "是".to_string() } else { "否（本地）".to_string() } }
                        }
                    }
                }
            }

            // MCP Client
            div { class: "bg-gray-800/50 rounded-lg p-4 border border-gray-700/50",
                h3 { class: "text-sm font-semibold text-gray-400 uppercase mb-3", "MCP 外部工具" }
                {
                    let tools = crate::mcp_client::get_discovered_tools();
                    if tools.is_empty() {
                        rsx! { p { class: "text-gray-600 text-sm", "未連接任何外部 MCP Server" } }
                    } else {
                        rsx! {
                            div { class: "space-y-1",
                                for tool in tools {
                                    div { class: "flex justify-between text-sm py-1",
                                        span { class: "text-blue-400", "{tool.registry_name()}" }
                                        span { class: "text-gray-500 truncate ml-2", "{tool.description}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Skills
            div { class: "bg-gray-800/50 rounded-lg p-4 border border-gray-700/50",
                h3 { class: "text-sm font-semibold text-gray-400 uppercase mb-3", "技能列表" }
                {
                    let skills = crate::skills::list_skills();
                    rsx! {
                        div { class: "space-y-1",
                            for skill in &skills {
                                div { class: "flex items-center gap-2 text-sm py-1",
                                    span { class: "text-xs px-1.5 py-0.5 rounded bg-gray-700 text-gray-400",
                                        "{skill.category}"
                                    }
                                    span { class: "text-white", "{skill.name}" }
                                    span { class: "text-gray-500 text-xs truncate ml-auto", "{skill.description}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn StatusRow(label: String, status: String, ok: bool) -> Element {
    let color = if ok { "text-green-400" } else { "text-red-400" };
    let dot = if ok { "●" } else { "○" };
    rsx! {
        div { class: "flex items-center justify-between",
            span { class: "text-sm text-gray-300", "{label}" }
            span { class: "text-sm {color}", "{dot} {status}" }
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
