//! Formatting helpers for the chat agent's dispatch layer.
//!
//! These turn tool-call outputs (file reports, skill catalogues, search
//! results) into markdown-flavoured reply strings for the user.  No LLM
//! calls or side effects — pure string transforms.

use serde_json::Value;

use crate::telegram::language::contains_cjk;

pub(super) fn extract_labeled_value<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
}

pub(super) fn summarize_file_report_line(report: &str) -> Option<String> {
    let path = extract_labeled_value(report, "File: ")?;
    let role = extract_labeled_value(report, "Role: ").unwrap_or("No summary available");
    let kind = extract_labeled_value(report, "Kind: ").unwrap_or("text");
    Some(format!("- `{path}`：{role}（{kind}）"))
}

pub(super) fn extract_excerpt_block(text: &str) -> Option<&str> {
    text.split_once("\nExcerpt:\n")
        .map(|(_, excerpt)| excerpt.trim())
        .filter(|excerpt| !excerpt.is_empty())
}

pub(super) fn code_fence_language(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "rs" => "rust",
        "toml" => "toml",
        "ts" => "ts",
        "tsx" => "tsx",
        "js" => "javascript",
        "jsx" => "jsx",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        _ => "text",
    }
}

fn truncate_for_reply(text: &str, max_lines: usize, max_chars: usize) -> String {
    let joined = text
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if joined.chars().count() <= max_chars {
        joined
    } else {
        let head: String = joined.chars().take(max_chars).collect();
        format!("{head}\n...")
    }
}

pub(super) fn format_local_file_reply(file_report: &str) -> Option<String> {
    let path = extract_labeled_value(file_report, "File: ")?;
    let kind = extract_labeled_value(file_report, "Kind: ").unwrap_or("text");
    let role = extract_labeled_value(file_report, "Role: ").unwrap_or("No summary available");
    let excerpt = truncate_for_reply(extract_excerpt_block(file_report)?, 20, 1400);
    let fence = code_fence_language(path);

    Some(format!(
        "我已實際讀取 `{path}`。\n- 類型：{kind}\n- 作用：{role}\n\n檔案片段：\n```{fence}\n{excerpt}\n```\n\n如果你要，我可以再繼續解釋這個檔案的結構或逐段說明。"
    ))
}

/// Map a raw category string from skills.rs to a user-friendly display label.
fn category_display_label(category: &str) -> &str {
    match category {
        "code-understanding" | "context-retrieval" => "理解 / 查詢程式碼",
        "code-optimization" => "分析 / 修正 / 驗證",
        "external-research" | "external" => "外部能力",
        other => other,
    }
}

pub(super) fn format_skill_catalog_reply(catalog: &Value, user_text: &str) -> Option<String> {
    let skills = catalog.as_array()?;
    if skills.is_empty() {
        return None;
    }

    let mut groups: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for skill in skills {
        let id = skill.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let category = skill
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("other")
            .to_string();
        groups.entry(category).or_default().push(format!("`{id}`"));
    }

    let use_chinese = contains_cjk(user_text);
    let sep = if use_chinese { "、" } else { ", " };

    let (header, footer) = if use_chinese {
        (
            "我目前在 `src/skills.rs` 裡有這些可用 skill：".to_string(),
            "如果你要，我可以直接示範其中一個，例如：`幫我看 src/main.rs`、`先分析再改`、`改完幫我測一下`。".to_string(),
        )
    } else {
        (
            "Here are the available skills defined in `src/skills.rs`:".to_string(),
            "I can demonstrate any of these — try: `show me src/main.rs`, `analyse then fix`, or `run tests after changes`.".to_string(),
        )
    };

    let mut lines = vec![header];
    for (category, ids) in &groups {
        lines.push(format!(
            "- {}: {}",
            category_display_label(category),
            ids.join(sep)
        ));
    }
    lines.push(footer);
    Some(lines.join("\n"))
}
