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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Issue #263 Phase 3: chat_agent/format.rs unit tests.
    // All functions here are pure string transforms — no LLM, no IO, no
    // global state — so each can be exercised with literal inputs.

    // ── extract_labeled_value ────────────────────────────────────────

    #[test]
    fn extract_labeled_value_finds_simple_match() {
        let text = "File: src/main.rs\nKind: text\nRole: entry";
        assert_eq!(extract_labeled_value(text, "File: "), Some("src/main.rs"));
        assert_eq!(extract_labeled_value(text, "Kind: "), Some("text"));
        assert_eq!(extract_labeled_value(text, "Role: "), Some("entry"));
    }

    #[test]
    fn extract_labeled_value_trims_whitespace() {
        let text = "File:   src/main.rs   \nKind: text";
        // strip_prefix takes "File: " (one trailing space); the rest is then
        // .trim()'d so multiple-space + trailing-space variants all collapse.
        assert_eq!(extract_labeled_value(text, "File: "), Some("src/main.rs"));
    }

    #[test]
    fn extract_labeled_value_none_when_value_empty() {
        let text = "File: \nKind: text";
        // empty value (only whitespace after prefix) → filtered out
        assert_eq!(extract_labeled_value(text, "File: "), None);
    }

    #[test]
    fn extract_labeled_value_none_when_prefix_absent() {
        let text = "Other: src/main.rs";
        assert_eq!(extract_labeled_value(text, "File: "), None);
    }

    // ── summarize_file_report_line ───────────────────────────────────

    #[test]
    fn summarize_file_report_line_renders_full_report() {
        let report = "File: src/lib.rs\nRole: 公開 API 入口\nKind: text\nExcerpt:\n...";
        let summary = summarize_file_report_line(report).unwrap();
        assert_eq!(summary, "- `src/lib.rs`：公開 API 入口（text）");
    }

    #[test]
    fn summarize_file_report_line_uses_defaults_when_role_kind_missing() {
        let report = "File: build.sh";
        let summary = summarize_file_report_line(report).unwrap();
        assert_eq!(summary, "- `build.sh`：No summary available（text）");
    }

    #[test]
    fn summarize_file_report_line_returns_none_when_no_path() {
        // Without a "File: " line, the helper bails entirely — there's
        // nothing meaningful to render.
        assert_eq!(summarize_file_report_line("Role: foo\nKind: text"), None);
    }

    // ── extract_excerpt_block ────────────────────────────────────────

    #[test]
    fn extract_excerpt_block_pulls_after_marker() {
        let text = "File: x\nKind: text\nExcerpt:\nfn main() {}";
        assert_eq!(extract_excerpt_block(text), Some("fn main() {}"));
    }

    #[test]
    fn extract_excerpt_block_none_when_marker_missing() {
        assert_eq!(extract_excerpt_block("File: x\nKind: text"), None);
    }

    #[test]
    fn extract_excerpt_block_none_when_excerpt_empty() {
        assert_eq!(extract_excerpt_block("File: x\nExcerpt:\n   \n"), None);
    }

    // ── code_fence_language ──────────────────────────────────────────

    #[test]
    fn code_fence_language_maps_known_extensions() {
        assert_eq!(code_fence_language("src/main.rs"), "rust");
        assert_eq!(code_fence_language("Cargo.toml"), "toml");
        assert_eq!(code_fence_language("build.ts"), "ts");
        assert_eq!(code_fence_language("App.tsx"), "tsx");
        assert_eq!(code_fence_language("index.js"), "javascript");
        assert_eq!(code_fence_language("App.jsx"), "jsx");
        assert_eq!(code_fence_language("config.json"), "json");
        assert_eq!(code_fence_language("doc.yaml"), "yaml");
        assert_eq!(code_fence_language("doc.yml"), "yaml");
    }

    #[test]
    fn code_fence_language_falls_back_to_text() {
        assert_eq!(code_fence_language("README.md"), "text");
        assert_eq!(code_fence_language("LICENSE"), "text");
        assert_eq!(code_fence_language(""), "text");
        assert_eq!(code_fence_language("file.unknown"), "text");
    }

    // ── truncate_for_reply (private but worth covering) ──────────────

    #[test]
    fn truncate_for_reply_respects_line_limit() {
        let text = "a\nb\nc\nd\ne";
        // Take first 3 lines only, well within char budget.
        assert_eq!(truncate_for_reply(text, 3, 1000), "a\nb\nc");
    }

    #[test]
    fn truncate_for_reply_appends_ellipsis_when_chars_exceed() {
        let text = "abcdefghij";
        // 1 line, 5 chars → "abcde\n..."
        let out = truncate_for_reply(text, 100, 5);
        assert!(out.starts_with("abcde"));
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncate_for_reply_no_ellipsis_when_under_budget() {
        // Exactly at budget: no "..." suffix
        let text = "hello";
        assert_eq!(truncate_for_reply(text, 10, 5), "hello");
    }

    // ── format_local_file_reply ──────────────────────────────────────

    #[test]
    fn format_local_file_reply_returns_full_message() {
        let report = "File: src/main.rs\nKind: text\nRole: entry point\nExcerpt:\nfn main() {}";
        let reply = format_local_file_reply(report).unwrap();
        // Spot-check the key components are present.
        assert!(reply.contains("`src/main.rs`"));
        assert!(reply.contains("類型：text"));
        assert!(reply.contains("作用：entry point"));
        assert!(reply.contains("```rust"));
        assert!(reply.contains("fn main() {}"));
    }

    #[test]
    fn format_local_file_reply_none_when_path_missing() {
        let report = "Kind: text\nExcerpt:\nfoo";
        assert!(format_local_file_reply(report).is_none());
    }

    #[test]
    fn format_local_file_reply_none_when_excerpt_missing() {
        let report = "File: src/main.rs\nKind: text\nRole: entry";
        assert!(format_local_file_reply(report).is_none());
    }

    // ── format_skill_catalog_reply ───────────────────────────────────

    #[test]
    fn format_skill_catalog_reply_groups_by_category() {
        let catalog = json!([
            {"id": "read_file",        "category": "code-understanding"},
            {"id": "search_code",      "category": "context-retrieval"},
            {"id": "run_tests",        "category": "code-optimization"},
            {"id": "fetch_url",        "category": "external-research"},
        ]);
        let reply = format_skill_catalog_reply(&catalog, "請告訴我有哪些 skill").unwrap();
        // CJK input → Chinese header / footer / separator
        assert!(reply.contains("我目前在 `src/skills.rs`"));
        // Both code-understanding and context-retrieval map to same display label —
        // BTreeMap groups them by their raw category strings, so each row appears
        // separately but with the same display label.
        assert!(reply.contains("理解 / 查詢程式碼"));
        assert!(reply.contains("分析 / 修正 / 驗證"));
        assert!(reply.contains("外部能力"));
        // CJK separator between IDs
        assert!(reply.contains("`read_file`") || reply.contains("`search_code`"));
    }

    #[test]
    fn format_skill_catalog_reply_uses_english_for_ascii_text() {
        let catalog = json!([
            {"id": "read_file", "category": "code-understanding"},
        ]);
        let reply = format_skill_catalog_reply(&catalog, "show me what skills exist").unwrap();
        assert!(reply.contains("Here are the available skills"));
        assert!(reply.contains("`read_file`"));
        // No CJK header/footer should leak in
        assert!(!reply.contains("我目前在"));
    }

    #[test]
    fn format_skill_catalog_reply_none_for_empty_array() {
        let catalog = json!([]);
        assert!(format_skill_catalog_reply(&catalog, "anything").is_none());
    }

    #[test]
    fn format_skill_catalog_reply_none_for_non_array() {
        let catalog = json!({"not": "an array"});
        assert!(format_skill_catalog_reply(&catalog, "anything").is_none());
    }

    #[test]
    fn format_skill_catalog_reply_skips_skills_without_id() {
        let catalog = json!([
            {"category": "code-understanding"},                  // no id → skipped
            {"id": "",        "category": "code-optimization"},  // empty id → skipped
            {"id": "valid",   "category": "code-optimization"},
        ]);
        let reply = format_skill_catalog_reply(&catalog, "list").unwrap();
        assert!(reply.contains("`valid`"));
        // Skipped entries shouldn't leave a visible mark.
        assert!(!reply.contains("``"));
    }
}
