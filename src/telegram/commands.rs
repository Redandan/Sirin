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
            let cleaned = q.trim().trim_matches('"').trim_matches('\'').trim().to_string();
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

    Some((if topic.is_empty() { normalized.to_string() } else { topic }, url))
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
    if normalized.contains("查詢待辦") || normalized.contains("列出待辦") || normalized.contains("看待辦") {
        let entries = match tracker.read_last_n(100) {
            Ok(v) => v,
            Err(e) => return Some(format!("執行結果：讀取待辦失敗，原因：{e}")),
        };

        let actionable: Vec<&TaskEntry> = entries
            .iter()
            .filter(|e| matches!(e.status.as_deref(), Some("PENDING") | Some("FOLLOWING") | Some("FOLLOWUP_NEEDED")))
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

        let target = entries
            .iter()
            .rev()
            .find(|e| matches!(e.status.as_deref(), Some("PENDING") | Some("FOLLOWING") | Some("FOLLOWUP_NEEDED")));

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
