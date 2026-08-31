//! Pure text/value helpers for the coding agent.
//!
//! These are stateless utilities: JSON extraction, output formatting,
//! preview trimming, task-string heuristics, and tool descriptions.
//! No dependency on `HistoryEntry` or agent runtime — move here to keep
//! `mod.rs` focused on the ReAct control flow.

#![allow(dead_code)]

use serde_json::Value;

// ── Text / JSON extraction ────────────────────────────────────────────────────

pub(super) fn extract_json_body(raw: &str) -> &str {
    let s = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        if start <= end {
            return &s[start..=end];
        }
    }
    s
}

/// Truncate `s` to at most `max_bytes` UTF-8 bytes, always at a valid char
/// boundary.  Chinese/CJK chars are 3 bytes each, so 72 bytes ≈ 24 CJK chars.
pub(super) fn truncate_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── Preview / formatting ──────────────────────────────────────────────────────

pub(super) fn preview_tool_input(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    s.chars().take(60).collect()
}

pub(super) fn preview_text(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

pub(super) fn format_tool_output(v: &Value) -> String {
    match v {
        Value::String(s) => s.chars().take(800).collect(),
        Value::Array(arr) => arr
            .iter()
            .take(10)
            .map(|item| {
                item.as_str()
                    .unwrap_or(&item.to_string())
                    .chars()
                    .take(120)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            let s = serde_json::to_string_pretty(other).unwrap_or_default();
            s.chars().take(800).collect()
        }
    }
}

/// Like `format_tool_output` but with a larger budget (2000 chars) for file_read
/// observations, so the LLM sees enough source context to construct accurate
/// `old_str` for `file_patch`.
pub(super) fn format_tool_output_large(v: &Value) -> String {
    match v {
        Value::String(s) => s.chars().take(2000).collect(),
        Value::Array(arr) => arr
            .iter()
            .take(20)
            .map(|item| {
                item.as_str()
                    .unwrap_or(&item.to_string())
                    .chars()
                    .take(200)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            let s = serde_json::to_string_pretty(other).unwrap_or_default();
            s.chars().take(2000).collect()
        }
    }
}

// ── Cache-key helpers ─────────────────────────────────────────────────────────

pub(super) fn step_fingerprint(
    action_name: &str,
    action_input: &Value,
    observation: &str,
) -> String {
    format!(
        "{}|{}|{}",
        action_name,
        preview_tool_input(action_input),
        preview_text(observation)
    )
}

pub(super) fn file_read_cache_key(input: &Value) -> String {
    let path = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let start = input.get("start_line").and_then(Value::as_u64).unwrap_or(0);
    let end = input.get("end_line").and_then(Value::as_u64).unwrap_or(0);

    format!("{path}|{start}|{end}")
}

// ── Task-string heuristics ────────────────────────────────────────────────────

pub(super) fn extract_path_hints_from_task(task: &str) -> Vec<String> {
    let mut hints = Vec::new();
    let known_exts = [
        ".rs", ".toml", ".md", ".json", ".yaml", ".yml", ".tsx", ".ts", ".js",
    ];

    for raw in task.split_whitespace() {
        let trimmed = raw
            .trim()
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '"'
                        | '\''
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | ','
                        | '，'
                        | '。'
                        | ':'
                        | '：'
                        | ';'
                        | '；'
                        | '?'
                        | '？'
                )
            })
            .replace('\\', "/");
        let cleaned: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
            .collect();

        if cleaned.is_empty() {
            continue;
        }

        let looks_like_path = cleaned.contains('/')
            || cleaned.starts_with("src")
            || cleaned.starts_with("tests")
            || cleaned.starts_with("app")
            || cleaned.starts_with("config");
        let has_known_ext = known_exts.iter().any(|ext| cleaned.ends_with(ext));

        if (looks_like_path || has_known_ext) && !hints.contains(&cleaned) {
            hints.push(cleaned);
        }

        if hints.len() >= 3 {
            break;
        }
    }

    hints
}

pub(super) fn build_task_named_file_context(path_hints: &[String]) -> String {
    let blocks: Vec<String> = path_hints
        .iter()
        .take(2)
        .filter_map(|path| {
            crate::memory::inspect_project_file_range(path, Some(1), Some(120), 5000)
                .ok()
                .map(|content| {
                    format!("\n\n## Task-named file hint\nRequested path: {path}\n{content}")
                })
        })
        .collect();

    blocks.join("")
}

pub(super) fn is_read_only_analysis_task(task: &str) -> bool {
    let lower = task.to_lowercase();
    let asks_analysis = [
        "分析", "說明", "summar", "explain", "inspect", "review", "找出", "檢查", "read ",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let forbids_write = [
        "不要寫入",
        "不要修改",
        "do not modify",
        "don't modify",
        "read-only",
        "dry-run",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let asks_to_change = [
        "修改",
        "新增",
        "加入",
        "修正",
        "fix",
        "implement",
        "write",
        "patch",
        "refactor",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    forbids_write || (asks_analysis && !asks_to_change)
}

// ── Tool catalogue ────────────────────────────────────────────────────────────

pub(super) fn describe_tools() -> String {
    let tools = [
        ("file_list", r#"{"path":"dir","max_depth":3}"#, "List files in a directory."),
        ("local_file_read", r#"{"path":"src/foo.rs","start_line":100,"end_line":200}"#, "Read a file's content. Use start_line/end_line (1-based, optional) to fetch a specific window — output includes line numbers, essential for precise file_patch old_str."),
        ("file_write", r#"{"path":"src/foo.rs","content":"..."}"#, "Write full content to a file (use only when replacing the entire file)."),
        ("file_patch", r#"{"path":"src/foo.rs","hunks":[{"old_str":"fn foo() {","new_str":"fn foo() -> i32 {"}]}"#, "Apply surgical hunk-based edits. Fails atomically if any old_str is not found. Prefer over file_write for partial changes."),
        ("file_diff", r#"{"path":null}"#, "Show git diff of uncommitted changes."),
        ("shell_exec", r#"{"command":"cargo check"}"#, "Run a whitelisted shell command."),
        ("codebase_search", r#"{"query":"...","limit":5}"#, "Search codebase for relevant code."),
        ("symbol_search", r#"{"query":"function_name"}"#, "Search for a symbol by name."),
        ("call_graph_query", r#"{"symbol":"my_fn","hops":1}"#, "Look up callers and callees of a symbol in the call graph."),
        ("plan_execute", r#"{"steps":[{"tool":"file_patch","input":{...}},{"tool":"shell_exec","input":{"command":"cargo check"}}]}"#, "Execute multiple tool steps in sequence. Stops on first failure. Use to batch multi-file changes in one action."),
        ("git_status", r#"{}"#, "Show git status."),
        ("git_log", r#"{"limit":5}"#, "Show recent git commits."),
        ("memory_search", r#"{"query":"...","limit":3}"#, "Search past memories."),
    ];
    tools
        .iter()
        .map(|(name, example, desc)| format!("- `{name}({example})`: {desc}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Tool-error enrichment ─────────────────────────────────────────────────────

pub(super) fn maybe_enrich_tool_error(action_name: &str, observation: String) -> String {
    if !observation.starts_with("ERROR:") {
        return observation;
    }

    let lower = observation.to_lowercase();
    let looks_like_path_issue = lower.contains("could not resolve local project file")
        || lower.contains("cannot read '")
        || lower.contains("patch aborted")
        || lower.contains("not found in '")
        || lower.contains("directory not found");

    if looks_like_path_issue
        && matches!(
            action_name,
            "local_file_read" | "file_patch" | "file_write" | "plan_execute"
        )
    {
        format!(
            "{observation}\nHint: verify the real path with file_list/codebase_search before more writes. In Rust projects, `foo.rs` may actually be `foo/mod.rs`."
        )
    } else {
        observation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Issue #263 Phase 3: helpers.rs pure-function tests.

    // ── extract_json_body ────────────────────────────────────────────

    #[test]
    fn extract_json_body_strips_markdown_fence() {
        assert_eq!(extract_json_body("```json\n{\"k\":1}\n```"), "{\"k\":1}");
        assert_eq!(extract_json_body("```\n{\"k\":1}\n```"), "{\"k\":1}");
    }

    #[test]
    fn extract_json_body_finds_outer_braces() {
        // Even with prose around, returns the {...} window.
        let s = "Here is the answer: {\"action\":\"x\"} thanks!";
        assert_eq!(extract_json_body(s), "{\"action\":\"x\"}");
    }

    #[test]
    fn extract_json_body_returns_input_when_no_braces() {
        assert_eq!(extract_json_body("plain text"), "plain text");
        assert_eq!(extract_json_body(""), "");
    }

    #[test]
    fn extract_json_body_handles_nested_objects() {
        let s = "```json\n{\"outer\":{\"inner\":42}}\n```";
        assert_eq!(extract_json_body(s), "{\"outer\":{\"inner\":42}}");
    }

    // ── truncate_to_bytes ────────────────────────────────────────────

    #[test]
    fn truncate_to_bytes_no_change_when_under_limit() {
        assert_eq!(truncate_to_bytes("hello", 10), "hello");
        assert_eq!(truncate_to_bytes("", 10), "");
    }

    #[test]
    fn truncate_to_bytes_respects_ascii_byte_limit() {
        assert_eq!(truncate_to_bytes("abcdefghij", 5), "abcde");
    }

    #[test]
    fn truncate_to_bytes_walks_back_to_char_boundary_for_cjk() {
        // "你好世界" = 4 chars × 3 bytes = 12 bytes total.
        // limit=5 cuts mid-char; helper must walk back to byte 3.
        let s = "你好世界";
        let out = truncate_to_bytes(s, 5);
        assert_eq!(out, "你"); // exactly one char, 3 bytes
                               // limit=8 → still mid-char (3rd char starts at 6, 4th at 9 → can't end at 8)
        assert_eq!(truncate_to_bytes(s, 8), "你好"); // 6 bytes
    }

    #[test]
    fn truncate_to_bytes_zero_limit_returns_empty() {
        assert_eq!(truncate_to_bytes("anything", 0), "");
    }

    // ── preview_tool_input / preview_text ────────────────────────────

    #[test]
    fn preview_tool_input_caps_at_60_chars() {
        let v = json!({"long": "x".repeat(500)});
        let out = preview_tool_input(&v);
        assert!(out.chars().count() <= 60);
    }

    #[test]
    fn preview_text_appends_ellipsis_when_over_120_chars() {
        let s: String = "a".repeat(200);
        let out = preview_text(&s);
        // 120 chars + "…"
        assert_eq!(out.chars().count(), 121);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn preview_text_no_ellipsis_when_under_120_chars() {
        let out = preview_text("short");
        assert_eq!(out, "short");
        assert!(!out.ends_with('…'));
    }

    #[test]
    fn preview_text_handles_cjk_correctly() {
        // 200 CJK chars — chars().take(120) is char-aware so should work.
        let s: String = "中".repeat(200);
        let out = preview_text(&s);
        assert_eq!(out.chars().count(), 121); // 120 + "…"
    }

    // ── format_tool_output ───────────────────────────────────────────

    #[test]
    fn format_tool_output_string_truncates_to_800_chars() {
        let v = Value::String("a".repeat(2000));
        let out = format_tool_output(&v);
        assert_eq!(out.chars().count(), 800);
    }

    #[test]
    fn format_tool_output_array_caps_items_and_chars() {
        let arr: Vec<Value> = (0..50).map(|i| json!(format!("item_{i}"))).collect();
        let out = format_tool_output(&Value::Array(arr));
        // First 10 items joined by '\n'
        let line_count = out.lines().count();
        assert_eq!(line_count, 10);
    }

    #[test]
    fn format_tool_output_object_pretty_prints_then_truncates() {
        let v = json!({"a": "x".repeat(2000)});
        let out = format_tool_output(&v);
        assert_eq!(out.chars().count(), 800);
        assert!(out.starts_with("{"));
    }

    #[test]
    fn format_tool_output_large_uses_2000_char_budget() {
        let v = Value::String("a".repeat(5000));
        let out = format_tool_output_large(&v);
        assert_eq!(out.chars().count(), 2000);
    }

    // ── step_fingerprint ─────────────────────────────────────────────

    #[test]
    fn step_fingerprint_combines_action_input_observation() {
        let fp = step_fingerprint("file_read", &json!({"path": "src/main.rs"}), "fn main() {}");
        // Format: "action|input_preview|obs_preview"
        assert!(fp.starts_with("file_read|"));
        assert!(fp.contains("src/main.rs"));
        assert!(fp.contains("fn main"));
        // Two pipes split into 3 segments
        assert_eq!(fp.matches('|').count(), 2);
    }

    #[test]
    fn step_fingerprint_collapses_long_input_via_preview() {
        let big = json!({"data": "x".repeat(500)});
        let fp = step_fingerprint("write", &big, "ok");
        // Whole fingerprint should still be small thanks to preview caps
        assert!(fp.len() < 250);
    }

    // ── file_read_cache_key ──────────────────────────────────────────

    #[test]
    fn file_read_cache_key_uses_path_or_file_path() {
        let k1 = file_read_cache_key(&json!({"path": "a.rs", "start_line": 10}));
        assert_eq!(k1, "a.rs|10|0");
        // file_path is the alt name (some tools use this)
        let k2 = file_read_cache_key(&json!({"file_path": "b.rs", "end_line": 20}));
        assert_eq!(k2, "b.rs|0|20");
    }

    #[test]
    fn file_read_cache_key_defaults_to_zero_for_missing_lines() {
        let k = file_read_cache_key(&json!({"path": "x.rs"}));
        assert_eq!(k, "x.rs|0|0");
    }

    #[test]
    fn file_read_cache_key_empty_when_no_path_field() {
        let k = file_read_cache_key(&json!({"other": "field"}));
        assert_eq!(k, "|0|0");
    }

    // ── extract_path_hints_from_task ─────────────────────────────────

    #[test]
    fn path_hints_extracts_explicit_paths() {
        let hints = extract_path_hints_from_task("Please read src/main.rs and tests/lib.rs");
        assert!(hints.contains(&"src/main.rs".to_string()));
        assert!(hints.contains(&"tests/lib.rs".to_string()));
    }

    #[test]
    fn path_hints_extracts_files_with_known_extensions() {
        let hints = extract_path_hints_from_task("Update Cargo.toml");
        assert!(hints.iter().any(|h| h.ends_with("Cargo.toml")));
    }

    #[test]
    fn path_hints_caps_at_three_results() {
        let hints = extract_path_hints_from_task("src/a.rs src/b.rs src/c.rs src/d.rs src/e.rs");
        assert!(hints.len() <= 3);
    }

    #[test]
    fn path_hints_strips_chinese_punctuation() {
        let hints = extract_path_hints_from_task("看看 `src/main.rs`，再分析一下");
        assert!(
            hints.contains(&"src/main.rs".to_string()),
            "must strip Chinese comma and backticks; got {:?}",
            hints
        );
    }

    #[test]
    fn path_hints_handles_backslash_to_slash_conversion() {
        let hints = extract_path_hints_from_task(r"check src\main.rs");
        assert!(hints.contains(&"src/main.rs".to_string()));
    }

    #[test]
    fn path_hints_dedupes_repeated_paths() {
        let hints = extract_path_hints_from_task("src/main.rs and src/main.rs again");
        assert_eq!(hints.iter().filter(|h| *h == "src/main.rs").count(), 1);
    }

    #[test]
    fn path_hints_empty_on_pure_prose() {
        let hints = extract_path_hints_from_task("explain how the system works");
        assert!(hints.is_empty());
    }

    // ── is_read_only_analysis_task ───────────────────────────────────

    #[test]
    fn read_only_when_explicit_forbid_phrase() {
        assert!(is_read_only_analysis_task("read-only audit pls"));
        assert!(is_read_only_analysis_task("don't modify the code"));
        assert!(is_read_only_analysis_task("不要寫入任何檔案"));
        assert!(is_read_only_analysis_task("dry-run mode only"));
    }

    #[test]
    fn read_only_when_analysis_words_without_change_words() {
        assert!(is_read_only_analysis_task("分析這個函數的行為"));
        // Avoid words containing "patch" / "fix" / "write" as substrings —
        // the source uses naive substring match, not tokenisation, so e.g.
        // "dispatcher" contains "patch" and would falsely trigger the
        // change-detector.  Stick to words known not to embed those needles.
        assert!(is_read_only_analysis_task("explain the function logic"));
        assert!(is_read_only_analysis_task("review the recent commit"));
    }

    #[test]
    fn not_read_only_when_change_words_present() {
        assert!(!is_read_only_analysis_task("分析然後修改 src/main.rs"));
        assert!(!is_read_only_analysis_task("explain and then implement"));
        assert!(!is_read_only_analysis_task("review and refactor"));
    }

    #[test]
    fn read_only_substring_match_quirk() {
        // Source uses substring containment, not tokenisation.  Document
        // the known false-negative this causes — words like "dispatcher"
        // contain "patch" and trigger asks_to_change → not classified as
        // read-only despite "explain" being present.  Future #263 follow-up
        // could switch to whole-word matching.
        assert!(
            !is_read_only_analysis_task("explain how the dispatcher works"),
            "'dispatcher' contains 'patch' → currently fails the read-only check"
        );
    }

    #[test]
    fn not_read_only_for_neutral_short_prompts() {
        // No analysis word AND no change word → falsy.
        assert!(!is_read_only_analysis_task("hello"));
        assert!(!is_read_only_analysis_task(""));
    }

    #[test]
    fn read_only_forbid_phrase_overrides_change_words() {
        // Even if "fix" appears, "do not modify" should win.
        assert!(is_read_only_analysis_task(
            "do not modify, just suggest a fix"
        ));
    }

    // ── describe_tools ───────────────────────────────────────────────

    #[test]
    fn describe_tools_renders_all_known_tools() {
        let out = describe_tools();
        // Spot-check: every tool name should appear in the output.
        for name in [
            "file_list",
            "local_file_read",
            "file_write",
            "file_patch",
            "file_diff",
            "shell_exec",
            "codebase_search",
            "symbol_search",
            "call_graph_query",
            "plan_execute",
            "git_status",
            "git_log",
            "memory_search",
        ] {
            assert!(out.contains(name), "missing tool: {name}");
        }
        // Format: each line starts with "- `name(...)`: description"
        for line in out.lines() {
            assert!(line.starts_with("- `"), "bad prefix on line: {line}");
        }
    }

    // ── maybe_enrich_tool_error ──────────────────────────────────────

    #[test]
    fn enrich_passthrough_when_not_an_error() {
        let obs = "OK".to_string();
        assert_eq!(maybe_enrich_tool_error("file_patch", obs.clone()), obs);
    }

    #[test]
    fn enrich_adds_path_hint_for_recognised_path_issue() {
        let obs = "ERROR: could not resolve local project file 'foo.rs'".to_string();
        let out = maybe_enrich_tool_error("local_file_read", obs);
        assert!(out.contains("Hint: verify the real path"));
        assert!(out.contains("foo/mod.rs"));
    }

    #[test]
    fn enrich_only_for_path_relevant_actions() {
        let obs = "ERROR: cannot read 'foo.rs'".to_string();
        // shell_exec is NOT in the gated list — pass through unchanged.
        assert_eq!(maybe_enrich_tool_error("shell_exec", obs.clone()), obs);
    }

    #[test]
    fn enrich_no_hint_for_unrelated_errors() {
        let obs = "ERROR: timeout after 30s".to_string();
        assert_eq!(
            maybe_enrich_tool_error("file_patch", obs.clone()),
            obs,
            "non-path errors should be returned untouched"
        );
    }

    #[test]
    fn enrich_recognises_each_path_phrase_variant() {
        let phrases = [
            "ERROR: could not resolve local project file 'x'",
            "ERROR: cannot read 'src/foo.rs'",
            "ERROR: patch aborted: hunk 0 not found",
            "ERROR: 'func' not found in 'src/lib.rs'",
            "ERROR: directory not found at /tmp/x",
        ];
        for p in phrases {
            let out = maybe_enrich_tool_error("file_patch", p.to_string());
            assert!(
                out.contains("Hint: verify the real path"),
                "phrase should trigger hint: {p}"
            );
        }
    }
}
