//! Workflow tab — AI Skill development pipeline (Define → Plan → Build → Verify → Review → Ship).

use dioxus::prelude::*;

use crate::workflow::{StageStatus, WorkflowState, STAGES};

#[component]
pub fn WorkflowView() -> Element {
    let wf_state: Signal<Option<WorkflowState>> = use_signal(|| WorkflowState::load());

    rsx! {
        div { class: "p-6 overflow-y-auto h-full space-y-6",
            h2 { class: "text-xl font-bold text-white", "Skill 開發工作流" }

            match &*wf_state.read() {
                None => rsx! { WorkflowEmpty { wf_state: wf_state } },
                Some(state) => rsx! { WorkflowActive { state: state.clone(), wf_state: wf_state } },
            }
        }
    }
}

#[component]
fn WorkflowEmpty(wf_state: Signal<Option<WorkflowState>>) -> Element {
    let mut feature = use_signal(String::new);
    let mut description = use_signal(String::new);

    rsx! {
        div { class: "bg-gray-800/50 rounded-lg p-6 border border-gray-700/50 max-w-lg mx-auto",
            h3 { class: "text-lg font-semibold text-white mb-4", "開始新的 Skill 開發" }

            div { class: "space-y-4",
                div {
                    label { class: "block text-sm text-gray-400 mb-1", "技能名稱" }
                    input {
                        class: "w-full bg-gray-900 border border-gray-700 rounded-lg px-3 py-2 text-white text-sm focus:border-blue-500 focus:outline-none",
                        placeholder: "例：VIP 維護",
                        value: "{feature}",
                        oninput: move |e| feature.set(e.value()),
                    }
                }
                div {
                    label { class: "block text-sm text-gray-400 mb-1", "功能描述" }
                    textarea {
                        class: "w-full bg-gray-900 border border-gray-700 rounded-lg px-3 py-2 text-white text-sm focus:border-blue-500 focus:outline-none h-24 resize-none",
                        placeholder: "描述這個技能要做什麼...",
                        value: "{description}",
                        oninput: move |e| description.set(e.value()),
                    }
                }
                button {
                    class: "w-full bg-blue-700 hover:bg-blue-600 text-white py-2 rounded-lg text-sm font-medium transition-colors",
                    disabled: feature.read().trim().is_empty(),
                    onclick: move |_| {
                        let f = feature.read().trim().to_string();
                        let d = description.read().trim().to_string();
                        if !f.is_empty() {
                            let skill_id = f.to_lowercase().replace(' ', "_");
                            let state = WorkflowState::new(&f, &d, &skill_id);
                            state.save();
                            wf_state.set(Some(state));
                        }
                    },
                    "🚀 開始開發"
                }
            }
        }
    }
}

#[component]
fn WorkflowActive(state: WorkflowState, wf_state: Signal<Option<WorkflowState>>) -> Element {
    rsx! {
        div { class: "space-y-6",
            // Header
            div { class: "flex items-center justify-between",
                div {
                    h3 { class: "text-lg font-bold text-white", "{state.feature}" }
                    if !state.description.is_empty() {
                        p { class: "text-sm text-gray-500 mt-1", "{state.description}" }
                    }
                }
                span { class: "text-xs text-gray-600", "開始於 {state.started_at}" }
            }

            // Stage pipeline
            div { class: "flex gap-2",
                for stage in STAGES {
                    {
                        let status = state.stage_status(stage.id);
                        let (bg, text_color, icon) = match status {
                            StageStatus::Done    => ("bg-green-900/50 border-green-700", "text-green-400", "✅"),
                            StageStatus::Current => ("bg-blue-900/50 border-blue-600", "text-blue-400", "▶"),
                            StageStatus::Pending => ("bg-gray-800/50 border-gray-700", "text-gray-500", "○"),
                        };
                        rsx! {
                            div { class: "flex-1 rounded-lg border p-3 text-center {bg}",
                                div { class: "text-lg", "{icon}" }
                                div { class: "text-xs font-medium {text_color} mt-1", "{stage.label}" }
                                div { class: "text-xs text-gray-600", "{stage.desc}" }
                            }
                        }
                    }
                }
            }

            // Current stage info
            if let Some(info) = state.current_stage_info() {
                div { class: "bg-gray-800/50 rounded-lg p-4 border border-blue-700/50",
                    h4 { class: "text-sm font-semibold text-blue-400", "當前階段: {info.label} — {info.desc}" }
                    if state.all_done() {
                        p { class: "text-green-400 mt-2", "🎉 所有階段已完成！" }
                    }
                }
            }

            // Reset button
            button {
                class: "text-xs text-gray-600 hover:text-red-400 transition-colors",
                onclick: move |_| {
                    let _ = std::fs::remove_file("data/workflow.json");
                    wf_state.set(None);
                },
                "🗑 重置工作流"
            }
        }
    }
}
