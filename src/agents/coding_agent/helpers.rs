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

pub(super) fn step_fingerprint(action_name: &str, action_input: &Value, observation: &str) -> String {
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
