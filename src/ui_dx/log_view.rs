//! Log view — system log with severity filters.

use dioxus::prelude::*;

use crate::log_buffer;

#[derive(Clone, Copy, PartialEq)]
enum LogFilter { All, WarnPlus, ErrorOnly }

#[component]
pub fn LogView() -> Element {
    let filter = use_signal(|| LogFilter::All);
    let mut cached_version = use_signal(|| 0usize);
    let mut cached_lines: Signal<Vec<(String, &'static str)>> = use_signal(Vec::new);

    // Refresh log lines when buffer or filter changes.
    let buf_ver = log_buffer::version();
    if buf_ver != *cached_version.read() {
        let all = log_buffer::recent(300);
        let f = *filter.read();
        let filtered: Vec<(String, &'static str)> = all
            .into_iter()
            .filter(|l| match f {
                LogFilter::All => true,
                LogFilter::WarnPlus => {
                    let lower = l.to_lowercase();
                    l.contains("[ERROR]") || l.contains("[WARN]") ||
                    lower.contains("error") || lower.contains("warn") || lower.contains("failed")
                }
                LogFilter::ErrorOnly => {
                    let lower = l.to_lowercase();
                    l.contains("[ERROR]") || lower.contains("error") || lower.contains("failed")
                }
            })
            .map(|line| {
                let color = classify_color(&line);
                (line, color)
            })
            .collect();
        cached_lines.set(filtered);
        cached_version.set(buf_ver);
    }

    let total = log_buffer::len();
    let lines = cached_lines.read().clone();
    let shown = lines.len();
    let f = *filter.read();

    rsx! {
        div { class: "flex flex-col h-full",
            // Header
            div { class: "flex items-center gap-3 p-4 border-b border-gray-800",
                h2 { class: "font-bold text-white", "系統 Log" }

                div { class: "flex gap-1",
                    { filter_btn("全部", LogFilter::All, f, filter, cached_version) }
                    { filter_btn("⚠ 警告+", LogFilter::WarnPlus, f, filter, cached_version) }
                    { filter_btn("✗ 錯誤", LogFilter::ErrorOnly, f, filter, cached_version) }
                }

                span { class: "text-xs text-gray-500 ml-auto",
                    if f == LogFilter::All { "{shown} 行" } else { "{shown} / {total} 行" }
                }

                button {
                    class: "text-xs px-2 py-1 bg-gray-800 hover:bg-gray-700 rounded text-gray-400",
                    onclick: move |_| log_buffer::clear(),
                    "🗑 清除"
                }
            }

            // Log lines
            div { class: "flex-1 overflow-y-auto p-4 font-mono text-xs space-y-0.5",
                if lines.is_empty() {
                    div { class: "text-gray-600 text-center py-8", "目前沒有符合條件的 Log" }
                }
                for (line, color_class) in &lines {
                    div { class: *color_class, "{line}" }
                }
            }
        }
    }
}

fn filter_btn(
    label: &str,
    target: LogFilter,
    current: LogFilter,
    mut filter: Signal<LogFilter>,
    mut version: Signal<usize>,
) -> Element {
    let is_active = current == target;
    let bg = if is_active { "bg-blue-700 text-white" } else { "bg-gray-800 text-gray-400 hover:bg-gray-700" };

    rsx! {
        button {
            class: "text-xs px-2 py-1 rounded {bg}",
            onclick: move |_| {
                filter.set(target);
                version.set(0);
            },
            "{label}"
        }
    }
}

fn classify_color(line: &str) -> &'static str {
    let lower = line.to_lowercase();
    if line.contains("[ERROR]") || lower.contains("error") || lower.contains("failed") {
        "text-red-400"
    } else if line.contains("[WARN]") || lower.contains("warn") {
        "text-yellow-400"
    } else if line.contains("[telegram]") || line.contains("[tg]") {
        "text-blue-400"
    } else if line.contains("[researcher]") {
        "text-green-400"
    } else if line.contains("[followup]") {
        "text-amber-400"
    } else if line.contains("[coding]") || line.contains("[adk]") {
        "text-purple-400"
    } else if line.contains("[teams]") {
        "text-cyan-400"
    } else {
        "text-gray-400"
    }
}
