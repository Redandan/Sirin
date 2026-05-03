//! tools/list ↔ tools/call parity guard (Issue #257, Option A).
//!
//! Sirin exposes 90+ MCP tools.  Each tool is currently defined in three
//! places in `src/mcp_server.rs`:
//!
//! 1. JSON literal in `handle_tools_list` (the `tools/list` response).
//! 2. Dispatch arm in `handle_tools_call` (the `tools/call` route).
//! 3. The `call_X` handler function itself.
//!
//! The most common bug is **silent drop**: a contributor adds a tool to
//! one of (1) / (2) but forgets the other.  `tools/list` advertises the
//! tool but `tools/call` returns "Unknown tool", or vice versa — the
//! tool exists at runtime but doesn't appear in the schema callers
//! probe to discover capabilities.
//!
//! This test reads `mcp_server.rs` as text at test time and panics if
//! the tool name set in (1) doesn't match the set in (2).
//!
//! ## Why text-grep instead of reflecting on the actual JSON
//!
//! `handle_tools_list` returns a `serde_json::Value`.  We could call it
//! from a test and parse the JSON.  But that requires the full Sirin
//! runtime context (database, http client, browser) to construct the
//! `AppService` / globals the function reads from.  Text parsing of the
//! source file is more brittle by 2 % and saves us 100 % of the
//! integration-harness setup.
//!
//! ## What this test does NOT catch
//!
//! - Schema field drift (handler reads `args["foo"]` but schema doesn't
//!   declare `foo`).  That needs Option B/C from the proposal.
//! - Description quality.
//! - Handler crashes.
//!
//! It catches *only* the name-set asymmetry — the cheap, common bug.

#![cfg(test)]

use std::collections::BTreeSet;
use std::path::Path;

/// Read the source file used by both extractors.  Skip silently when not
/// running from the crate root (sub-crate / partial-checkout builds).
fn read_mcp_server_source() -> Option<String> {
    let path = Path::new("src").join("mcp_server.rs");
    std::fs::read_to_string(&path).ok()
}

/// Extract the slice of `mcp_server.rs` between `start_marker` and
/// `end_marker` (exclusive).  Both markers must appear exactly once;
/// returns `None` otherwise.
fn slice_between<'a>(src: &'a str, start_marker: &str, end_marker: &str) -> Option<&'a str> {
    let start = src.find(start_marker)?;
    let after_start = &src[start..];
    let end = after_start.find(end_marker)?;
    Some(&after_start[..end])
}

/// Tool-list extractor: pulls every `"name": "X",` from the
/// `handle_tools_list` body.  The trailing comma rules out
/// `"name": { ... }` (input-schema fields whose property name happens
/// to be "name" — see e.g. `persist_adhoc_run`).
fn extract_listed_tool_names(src: &str) -> BTreeSet<String> {
    let body = match slice_between(
        src,
        "fn handle_tools_list",
        "// ── tools/call ",
    ) {
        Some(b) => b,
        None => return BTreeSet::new(),
    };

    let mut out = BTreeSet::new();
    // Pattern: `"name": "<snake_case>"` followed by an optional comma.
    // Trailing comma is the discriminator vs schema-field "name".
    let pat = "\"name\":";
    let mut rest = body;
    while let Some(idx) = rest.find(pat) {
        let after = &rest[idx + pat.len()..];
        // Skip whitespace after the colon.
        let after_trim = after.trim_start();
        // The next char must be a `"` (string value, not `{` for nested objects).
        if !after_trim.starts_with('"') {
            rest = after;
            continue;
        }
        // Extract the string literal contents.
        let after_quote = &after_trim[1..];
        if let Some(end) = after_quote.find('"') {
            let raw = &after_quote[..end];
            // The character after the closing quote must be `,` to count
            // (object-property terminator).  Tools always have at least
            // `description` after `name`; nested input-schema fields named
            // "name" do too, so this isn't a perfect filter — but combined
            // with the snake_case requirement below it works.
            let after_close = &after_quote[end + 1..];
            let next_non_ws = after_close.chars().find(|c| !c.is_whitespace());
            if next_non_ws == Some(',')
                && raw.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !raw.is_empty()
            {
                out.insert(raw.to_string());
            }
            rest = &after_quote[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// Dispatch-arm extractor: pulls every `"X" =>` arm from the
/// `handle_tools_call` body.  Catches both the JSON match block (returns
/// `Result<Value, String>`) and the trailing text-mode match block.
fn extract_dispatched_tool_names(src: &str) -> BTreeSet<String> {
    // The function is bounded by its `async fn` declaration and the next
    // top-level `fn ` / `async fn ` definition.  Use a stable landmark.
    let body = match slice_between(
        src,
        "async fn handle_tools_call",
        // The bare-ASCII landmark "// ── Tool implementations" lives just
        // after handle_tools_call's closing brace.
        "// ── Tool implementations",
    ) {
        Some(b) => b,
        None => return BTreeSet::new(),
    };

    let mut out = BTreeSet::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('"') {
            continue;
        }
        // Pattern: `"X" =>` — extract X.
        let after_quote = &trimmed[1..];
        let Some(end) = after_quote.find('"') else { continue; };
        let name = &after_quote[..end];
        // Must be snake_case.
        if name.is_empty()
            || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            continue;
        }
        // Must be followed by `=>` (allowing for whitespace).
        let after = after_quote[end + 1..].trim_start();
        if !after.starts_with("=>") {
            continue;
        }
        out.insert(name.to_string());
    }
    out
}

#[test]
fn tools_list_and_dispatch_in_sync() {
    let Some(src) = read_mcp_server_source() else {
        // Sub-crate build with no src/mcp_server.rs — skip.
        return;
    };

    let listed     = extract_listed_tool_names(&src);
    let dispatched = extract_dispatched_tool_names(&src);

    // Sanity: the extractors should find non-trivial counts.  If we hit 0
    // it means the landmark strings drifted and the test is silently
    // useless.  Fail loud.
    assert!(
        listed.len()     >= 50,
        "extract_listed_tool_names found only {} tool(s) — landmark strings may have drifted",
        listed.len(),
    );
    assert!(
        dispatched.len() >= 50,
        "extract_dispatched_tool_names found only {} arm(s) — landmark strings may have drifted",
        dispatched.len(),
    );

    let only_listed: Vec<_> = listed.difference(&dispatched).cloned().collect();
    let only_dispatched: Vec<_> = dispatched.difference(&listed).cloned().collect();

    if !only_listed.is_empty() || !only_dispatched.is_empty() {
        let listed_msg = if only_listed.is_empty() {
            "(none)".to_string()
        } else {
            only_listed.iter().map(|n| format!("  - {n}")).collect::<Vec<_>>().join("\n")
        };
        let dispatched_msg = if only_dispatched.is_empty() {
            "(none)".to_string()
        } else {
            only_dispatched.iter().map(|n| format!("  - {n}")).collect::<Vec<_>>().join("\n")
        };
        panic!(
            "tools/list ↔ tools/call out of sync (#257 silent-drop guard):\n\n\
             Advertised in tools/list but no dispatch arm in tools/call:\n{listed_msg}\n\n\
             Dispatched but not advertised:\n{dispatched_msg}\n\n\
             Fix: add the missing entry to {}.",
            if !only_listed.is_empty() { "handle_tools_call" } else { "handle_tools_list" },
        );
    }
}

#[cfg(test)]
mod extractor_tests {
    use super::*;

    #[test]
    fn listed_extractor_picks_up_simple_tool() {
        let src = r#"
fn handle_tools_list() -> Result<Value, String> {
    Ok(json!({
        "tools": [
            {
                "name": "memory_search",
                "description": "...",
                "inputSchema": { "type": "object" }
            }
        ]
    }))
}

// ── tools/call ────────────────────────────────────────────────────────────────
"#;
        let names = extract_listed_tool_names(src);
        assert!(names.contains("memory_search"));
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn listed_extractor_skips_schema_field_named_name() {
        // The `persist_adhoc_run` tool has an input-schema field literally
        // called "name".  The trailing-comma + snake_case filter must not
        // confuse it with a tool name.
        let src = r#"
fn handle_tools_list() -> Result<Value, String> {
    Ok(json!({
        "tools": [
            {
                "name": "persist_adhoc_run",
                "description": "...",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "..." }
                    }
                }
            }
        ]
    }))
}

// ── tools/call ────────────────────────────────────────────────────────────────
"#;
        let names = extract_listed_tool_names(src);
        assert_eq!(names, ["persist_adhoc_run"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn dispatched_extractor_picks_up_both_match_blocks() {
        let src = r#"
async fn handle_tools_call(params: Value, user_agent: &str) -> Result<Value, String> {
    match name {
        "list_tests"  => return call_list_tests(arguments).map(wrap_json),
        "run_test"    => return call_run_test(arguments).map(wrap_json),
        _ => {}
    }

    let text = match name {
        "memory_search"    => call_memory_search(arguments).await?,
        "skill_list"       => call_skill_list(),
        other => return Err(format!("Unknown tool: {other}")),
    };
    Ok(json!({}))
}

// ── Tool implementations ──────────────────────────────────────────────────────
"#;
        let names = extract_dispatched_tool_names(src);
        assert!(names.contains("list_tests"));
        assert!(names.contains("run_test"));
        assert!(names.contains("memory_search"));
        assert!(names.contains("skill_list"));
        // `other` is not snake_case-quoted; correctly excluded.
        assert!(!names.contains("other"));
        assert_eq!(names.len(), 4);
    }
}
