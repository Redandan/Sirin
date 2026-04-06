//! User command dispatch — parses inbound Telegram message text and executes
//! built-in actions (todo CRUD, task queries, research intent detection).

use chrono::Utc;
use std::collections::HashMap;

use crate::persona::{TaskEntry, TaskTracker};

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn message_preview(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

/// Use the local LLM to extract a concise search query from a natural-language message.
///
/// Falls back to the raw text (truncated) if the LLM call fails.
pub async fn extract_search_query(
    client: &reqwest::Client,
    llm: &crate::llm::LlmConfig,
    text: &str,
) -> String {
    let prompt = format!(
        "Extract a concise web search query (at most 8 words) from the user message below.\n\
Return only the search query text. No explanation, no quotes, no punctuation at the end.\n\
\n\
User message: {text}\n\
\n\
Search query:"
    );

    match crate::llm::call_prompt(client, llm, prompt).await {
        Ok(q) => {
            let cleaned = q
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string();
            if cleaned.is_empty() {
                text.chars().take(100).collect()
            } else {
                cleaned
            }
        }
        Err(_) => text.chars().take(100).collect(),
    }
}

/// Returns `true` when the message looks like a question that could benefit
/// from a web search.
pub fn should_search(text: &str) -> bool {
    let lower = text.to_lowercase();
    text.contains('?')
        || text.contains('？')
        || lower.contains("什麼")
        || lower.contains("如何")
        || lower.contains("為什麼")
        || lower.contains("怎麼")
        || lower.contains("哪裡")
        || lower.contains("what")
        || lower.contains("how")
        || lower.contains("why")
        || lower.contains("when")
        || lower.contains("where")
        || lower.contains("who")
}

/// Detect if a message is a research request.
///
/// Returns `Some((topic, url))` when the message starts with a research keyword.
/// The URL is extracted from the message if present.
pub fn detect_research_intent(text: &str) -> Option<(String, Option<String>)> {
    let normalized = text.trim();
    let lower = normalized.to_lowercase();

    let is_research = lower.starts_with("調研")
        || lower.starts_with("研究")
        || lower.starts_with("幫我研究")
        || lower.starts_with("幫我調研")
        || lower.starts_with("幫我查一下")
        || lower.starts_with("幫我查")
        || lower.starts_with("深入研究")
        || lower.starts_with("背景調研");

    if !is_research {
        return None;
    }

    // Extract URL using simple pattern matching.
    let url = normalized
        .split_whitespace()
        .find(|token| token.starts_with("http://") || token.starts_with("https://"))
        .map(|s| s.to_string());

    // The topic is the full message text, trimmed of the keyword.
    let topic = normalized
        .trim_start_matches("幫我調研")
        .trim_start_matches("幫我研究")
        .trim_start_matches("幫我查一下")
        .trim_start_matches("幫我查")
        .trim_start_matches("深入研究")
        .trim_start_matches("背景調研")
        .trim_start_matches("調研")
        .trim_start_matches("研究")
        .trim()
        .to_string();

    Some((
        if topic.is_empty() {
            normalized.to_string()
        } else {
            topic
        },
        url,
    ))
}

/// Execute simple user commands from Telegram message text and return
/// a human-readable execution report.
pub fn execute_user_request(
    text: &str,
    tracker: &TaskTracker,
    persona_name: &str,
) -> Option<String> {
    let normalized = text.trim();
    if normalized.is_empty() {
        return None;
    }

    let lower = normalized.to_lowercase();

    // 1) Create a pending task from explicit user instruction.
    if lower.starts_with("todo ")
        || normalized.starts_with("待辦")
        || normalized.starts_with("記錄任務")
        || normalized.starts_with("幫我記錄")
    {
        let detail = normalized
            .trim_start_matches("todo")
            .trim_start_matches('：')
            .trim_start_matches(':')
            .trim();

        let entry = TaskEntry {
            timestamp: Utc::now().to_rfc3339(),
            event: "user_request".to_string(),
            persona: persona_name.to_string(),
            correlation_id: None,
            message_preview: Some(message_preview(normalized, 140)),
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: Some("PENDING".to_string()),
            reason: Some(if detail.is_empty() {
                normalized.to_string()
            } else {
                detail.to_string()
            }),
            action_tier: None,
            high_priority: None,
        };

        return match tracker.record(&entry) {
            Ok(_) => Some("執行結果：已幫你建立待辦，狀態為 PENDING。".to_string()),
            Err(e) => Some(format!("執行結果：建立待辦失敗，原因：{e}")),
        };
    }

    // 2) Query actionable tasks.
    if normalized.contains("查詢待辦")
        || normalized.contains("列出待辦")
        || normalized.contains("看待辦")
    {
        let entries = match tracker.read_last_n(100) {
            Ok(v) => v,
            Err(e) => return Some(format!("執行結果：讀取待辦失敗，原因：{e}")),
        };

        let actionable: Vec<&TaskEntry> = entries
            .iter()
            .filter(|e| {
                matches!(
                    e.status.as_deref(),
                    Some("PENDING") | Some("FOLLOWING") | Some("FOLLOWUP_NEEDED")
                )
            })
            .collect();

        if actionable.is_empty() {
            return Some("執行結果：目前沒有待辦任務。".to_string());
        }

        let preview = actionable
            .iter()
            .take(3)
            .map(|e| {
                let status = e.status.as_deref().unwrap_or("?");
                let reason = e.reason.as_deref().unwrap_or("(無描述)");
                format!("- {status}: {reason}")
            })
            .collect::<Vec<_>>()
            .join("\n");

        return Some(format!(
            "執行結果：目前共有 {} 筆待辦。\n{}",
            actionable.len(),
            preview
        ));
    }

    // 3) Complete the latest pending task.
    if normalized.contains("完成最新待辦") || normalized.contains("完成待辦") {
        let entries = match tracker.read_last_n(200) {
            Ok(v) => v,
            Err(e) => return Some(format!("執行結果：讀取待辦失敗，原因：{e}")),
        };

        let target = entries.iter().rev().find(|e| {
            matches!(
                e.status.as_deref(),
                Some("PENDING") | Some("FOLLOWING") | Some("FOLLOWUP_NEEDED")
            )
        });

        if let Some(item) = target {
            let mut updates = HashMap::new();
            updates.insert(item.timestamp.clone(), "DONE".to_string());
            return match tracker.update_statuses(&updates) {
                Ok(_) => Some("執行結果：已將最新待辦標記為 DONE。".to_string()),
                Err(e) => Some(format!("執行結果：更新待辦失敗，原因：{e}")),
            };
        }

        return Some("執行結果：沒有可完成的待辦。".to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── message_preview ───────────────────────────────────────────────────────

    #[test]
    fn preview_truncates_long_text() {
        let text = "a".repeat(200);
        let p = message_preview(&text, 80);
        assert!(p.ends_with("..."), "should add ellipsis");
        // body is exactly 80 chars, then "..." = 83
        assert!(p.len() <= 83);
    }

    #[test]
    fn preview_passes_short_text_unchanged() {
        assert_eq!(message_preview("hello world", 80), "hello world");
    }

    #[test]
    fn preview_normalizes_whitespace() {
        assert_eq!(message_preview("hello   world", 80), "hello world");
    }

    // ── should_search ─────────────────────────────────────────────────────────

    #[test]
    fn should_search_detects_question_marks() {
        assert!(should_search("Rust 是什麼?"));
        assert!(should_search("How does async work？"));
    }

    #[test]
    fn should_search_detects_chinese_keywords() {
        assert!(should_search("Rust 是什麼"));
        assert!(should_search("如何使用 tokio"));
        assert!(should_search("為什麼需要 lifetime"));
        assert!(should_search("怎麼實作 async"));
        assert!(should_search("哪裡可以找到文件"));
    }

    #[test]
    fn should_search_detects_english_keywords() {
        assert!(should_search("what is async"));
        assert!(should_search("how does this work"));
        assert!(should_search("why is Rust safe"));
        assert!(should_search("when did Rust release"));
        assert!(should_search("where is the config file"));
        assert!(should_search("who wrote this code"));
    }

    #[test]
    fn should_search_rejects_plain_commands() {
        assert!(!should_search("幫我修改 src/llm.rs"));
        assert!(!should_search("add a feature to the router"));
        assert!(!should_search("refactor this function"));
    }

    // ── detect_research_intent ───────────────────────────────────────────────

    #[test]
    fn research_intent_accepts_all_trigger_prefixes() {
        let cases = [
            ("調研 Rust async", "Rust async"),
            ("研究 Rust async", "Rust async"),
            ("幫我研究 Rust async", "Rust async"),
            ("幫我調研 Rust async", "Rust async"),
            ("幫我查一下 Rust async", "Rust async"),
            ("幫我查 Rust async", "Rust async"),
            ("深入研究 Rust async", "Rust async"),
            ("背景調研 Rust async", "Rust async"),
        ];
        for (text, expected_topic_fragment) in &cases {
            let result = detect_research_intent(text);
            assert!(result.is_some(), "should match: {text}");
            let (topic, url) = result.unwrap();
            assert!(
                topic.contains(expected_topic_fragment),
                "topic should contain '{expected_topic_fragment}', got: {topic}"
            );
            assert_eq!(url, None, "no URL in: {text}");
        }
    }

    #[test]
    fn research_intent_extracts_url() {
        let result = detect_research_intent("幫我研究 https://example.com 的功能");
        assert!(result.is_some());
        let (_topic, url) = result.unwrap();
        assert_eq!(url, Some("https://example.com".to_string()));
    }

    #[test]
    fn research_intent_extracts_http_url() {
        let result = detect_research_intent("調研 http://localhost:8080 的 API");
        let (_topic, url) = result.unwrap();
        assert_eq!(url, Some("http://localhost:8080".to_string()));
    }

    #[test]
    fn research_intent_rejects_non_research() {
        assert!(detect_research_intent("你是誰").is_none());
        assert!(detect_research_intent("幫我修改 llm.rs").is_none());
        assert!(detect_research_intent("hello world").is_none());
        assert!(detect_research_intent("").is_none());
    }

    // ── execute_user_request ─────────────────────────────────────────────────

    fn tmp_tracker(suffix: &str) -> (crate::persona::TaskTracker, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "sirin_cmd_test_{}_{}.jsonl",
            std::process::id(),
            suffix
        ));
        (crate::persona::TaskTracker::new(&path), path)
    }

    #[test]
    fn execute_creates_todo_and_records_pending() {
        let (tracker, path) = tmp_tracker("create");
        let result = execute_user_request("todo 測試任務描述", &tracker, "Sirin");
        assert!(result.is_some());
        assert!(result.unwrap().contains("PENDING"));
        let entries = tracker.read_last_n(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status.as_deref(), Some("PENDING"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn execute_todo_chinese_prefix_also_records() {
        let (tracker, path) = tmp_tracker("zh_todo");
        let result = execute_user_request("待辦 買咖啡", &tracker, "Sirin");
        assert!(result.is_some(), "待辦 prefix should be recognized");
        let entries = tracker.read_last_n(10).unwrap();
        assert!(!entries.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn execute_query_returns_empty_when_no_todos() {
        let (tracker, path) = tmp_tracker("query_empty");
        let result = execute_user_request("查詢待辦", &tracker, "Sirin");
        assert!(result.is_some());
        assert!(result.unwrap().contains("沒有"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn execute_query_lists_pending_todos() {
        let (tracker, path) = tmp_tracker("query_list");
        execute_user_request("todo 任務A", &tracker, "Sirin");
        let result = execute_user_request("查詢待辦", &tracker, "Sirin");
        let msg = result.unwrap();
        assert!(
            msg.contains("PENDING") || msg.contains("待辦"),
            "should list tasks: {msg}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn execute_complete_latest_marks_done() {
        let (tracker, path) = tmp_tracker("complete");
        execute_user_request("todo 待完成任務", &tracker, "Sirin");
        let result = execute_user_request("完成最新待辦", &tracker, "Sirin");
        assert!(result.is_some());
        assert!(result.unwrap().contains("DONE"));
        let entries = tracker.read_last_n(10).unwrap();
        assert_eq!(entries[0].status.as_deref(), Some("DONE"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn execute_returns_none_for_unrecognized_input() {
        let (tracker, path) = tmp_tracker("none");
        let result = execute_user_request("你好", &tracker, "Sirin");
        assert_eq!(result, None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn execute_complete_when_nothing_pending_says_so() {
        let (tracker, path) = tmp_tracker("no_pending");
        let result = execute_user_request("完成最新待辦", &tracker, "Sirin");
        assert!(result.is_some());
        assert!(result.unwrap().contains("沒有"));
        std::fs::remove_file(&path).ok();
    }
}
