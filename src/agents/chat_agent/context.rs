//! Context assembly for chat responses.
//!
//! Gathers conversation history, memory snippets, search results, and local
//! file contents so that the LLM has the right information to answer.

use serde_json::{json, Value};

use crate::adk::AgentContext;
use crate::memory::load_recent_context;
use crate::telegram::commands::should_search;

use super::intent::{is_simple_meta_request, related_research_snippet};
use super::ChatRequest;

// ── Context builders ──────────────────────────────────────────────────────────

/// Return the conversation history block to prepend to prompts.
///
/// Uses `request.context_block` verbatim when present; otherwise loads the
/// last 5 turns from the per-peer conversation log.
pub(super) fn resolve_context_block(request: &ChatRequest, ctx: &AgentContext) -> Option<String> {
    if request.context_block.is_some() {
        return request.context_block.clone();
    }

    if is_simple_meta_request(&request.user_text) {
        return None;
    }

    match load_recent_context(5, request.peer_id, request.agent_id.as_deref()) {
        Ok(entries) if !entries.is_empty() => {
            ctx.record_system_event(
                "adk_chat_context_loaded",
                Some(super::preview_text(&request.user_text)),
                Some("RUNNING"),
                Some(format!("history_entries={}", entries.len())),
            );
            Some(
                entries
                    .iter()
                    .map(|e| format!("User: {}\nAssistant: {}", e.user_msg, e.assistant_reply))
                    .collect::<Vec<_>>()
                    .join("\n---\n"),
            )
        }
        Ok(_) => None,
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_context_error",
                Some(super::preview_text(&request.user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err.to_string()),
            );
            None
        }
    }
}

/// Perform a web search when the request warrants one and return a formatted
/// results block.  Returns `None` when search is skipped or fails.
///
/// Query extraction uses the router (local) LLM — it's a simple ≤8-word
/// extraction task that doesn't need a remote model.
pub(super) async fn resolve_search_context(
    request: &ChatRequest,
    ctx: &AgentContext,
    direct_answer_request: bool,
) -> Option<String> {
    if direct_answer_request || !should_search(&request.user_text) {
        return None;
    }

    let query = extract_search_query_via_caller(ctx, &request.user_text).await;
    match ctx
        .call_tool("web_search", json!({ "query": query.clone(), "limit": 3 }))
        .await
    {
        Ok(results) => {
            let formatted = format_search_results(&results);
            if let Some(ref block) = formatted {
                let result_count = block.lines().count();
                ctx.record_system_event(
                    "adk_chat_search",
                    Some(super::preview_text(&request.user_text)),
                    Some("RUNNING"),
                    Some(format!("query={query}, result_lines={result_count}")),
                );
            }
            formatted
        }
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_search_error",
                Some(super::preview_text(&request.user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err),
            );
            None
        }
    }
}

/// Search the memory store and fetch any related research, returning a
/// combined context block.  Returns `None` when nothing relevant is found.
pub(super) async fn resolve_memory_context(user_text: &str, ctx: &AgentContext) -> Option<String> {
    let mut blocks = Vec::new();
    let mut notes = Vec::new();

    match ctx
        .call_tool("memory_search", json!({ "query": user_text, "limit": 2 }))
        .await
    {
        Ok(results) => {
            if let Some(entries) = results.as_array() {
                let lines: Vec<String> = entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect();
                if !lines.is_empty() {
                    notes.push(format!("memory_hits={}", lines.len()));
                    blocks.push(lines.join("\n\n---\n\n"));
                }
            }
        }
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_memory_error",
                Some(super::preview_text(user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err),
            );
        }
    }

    if let Some(report) = related_research_snippet(user_text) {
        notes.push("research_hit=1".to_string());
        blocks.push(format!("Recent related research:\n{report}"));
    }

    if !notes.is_empty() {
        ctx.record_system_event(
            "adk_chat_memory_loaded",
            Some(super::preview_text(user_text)),
            Some("RUNNING"),
            Some(notes.join(", ")),
        );
    }

    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n---\n\n"))
    }
}

/// Read up to `max_files` local file reports via the `local_file_read` tool,
/// returning the raw content strings for each file successfully read.
pub(super) async fn load_local_file_reports(
    ctx: &AgentContext,
    user_text: &str,
    paths: &[String],
    max_files: usize,
    max_chars: usize,
) -> Vec<String> {
    let mut reports = Vec::new();

    for path in paths.iter().take(max_files) {
        match ctx
            .call_tool(
                "local_file_read",
                json!({ "path": path, "max_chars": max_chars }),
            )
            .await
        {
            Ok(result) => {
                if let Some(content) = result.get("content").and_then(Value::as_str) {
                    reports.push(content.to_string());
                }
            }
            Err(err) => {
                ctx.record_system_event(
                    "adk_chat_local_file_read_error",
                    Some(super::preview_text(user_text)),
                    Some("FOLLOWUP_NEEDED"),
                    Some(err),
                );
            }
        }
    }

    if !reports.is_empty() {
        ctx.record_system_event(
            "adk_chat_local_files_inspected",
            Some(super::preview_text(user_text)),
            Some("RUNNING"),
            Some(format!("files={}", reports.len())),
        );
    }

    reports
}

/// Ask the router LLM for a concise (≤ 8-word) search query distilled from
/// the user's message.  Falls back to a 100-char truncation of the original
/// text on LLM error or empty response.
async fn extract_search_query_via_caller(ctx: &AgentContext, user_text: &str) -> String {
    let prompt = format!(
        "Extract a concise web search query (at most 8 words) from the user message below.\n\
Return only the search query text. No explanation, no quotes, no punctuation at the end.\n\
\n\
User message: {user_text}\n\
\n\
Search query:"
    );

    match ctx.llm_caller.call_router(prompt).await {
        Ok(q) => {
            let cleaned = q
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string();
            if cleaned.is_empty() {
                user_text.chars().take(100).collect()
            } else {
                cleaned
            }
        }
        Err(_) => user_text.chars().take(100).collect(),
    }
}

/// Format raw web-search tool output into a human-readable block.
pub(super) fn format_search_results(results: &Value) -> Option<String> {
    let lines: Vec<String> = results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let title = item.get("title").and_then(Value::as_str)?.trim();
            let snippet = item
                .get("snippet")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            let url = item
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();

            if title.is_empty() {
                None
            } else {
                Some(format!("- {title}: {snippet} ({url})"))
            }
        })
        .collect();

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// #263 Phase 2 — coverage for the LLM-driven query extraction path.  Two
// surfaces are exercised:
//   1. `extract_search_query_via_caller` — the helper that does the actual
//      LLM call + sanitisation.  Tested directly because that's where the
//      branching lives (trim, quote-strip, empty fallback, error fallback).
//   2. `resolve_search_context` — the public entry point.  Pinned for the
//      two short-circuit paths (no LLM call) since the happy path needs a
//      registered `web_search` tool which the test ToolRegistry doesn't have.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::adk::tool::ToolRegistry;
    use crate::adk::AgentContext;
    use crate::llm::{LlmKind, MockLlmCaller};

    fn ctx_with_mock(mock: Arc<MockLlmCaller>) -> AgentContext {
        AgentContext::new("test-context", ToolRegistry::new()).with_llm_caller(mock)
    }

    // ── extract_search_query_via_caller ────────────────────────────────────

    #[tokio::test]
    async fn extract_search_query_returns_trimmed_response_on_success() {
        let mock = Arc::new(MockLlmCaller::with_responses(vec![Ok(
            "  rust async runtime  \n".to_string(),
        )]));
        let ctx = ctx_with_mock(Arc::clone(&mock));

        let q = extract_search_query_via_caller(&ctx, "tell me about rust async runtimes").await;
        assert_eq!(q, "rust async runtime");
        assert_eq!(mock.call_count(), 1);
    }

    #[tokio::test]
    async fn extract_search_query_strips_surrounding_quotes() {
        // Models often wrap the query in quotes despite being told not to.
        let mock = Arc::new(MockLlmCaller::with_responses(vec![Ok(
            "\"hello world\"".to_string(),
        )]));
        let ctx = ctx_with_mock(Arc::clone(&mock));

        let q = extract_search_query_via_caller(&ctx, "say hi").await;
        assert_eq!(q, "hello world");
    }

    #[tokio::test]
    async fn extract_search_query_falls_back_to_truncated_text_on_llm_error() {
        let mock = Arc::new(MockLlmCaller::with_responses(vec![Err(
            "simulated 500".to_string(),
        )]));
        let ctx = ctx_with_mock(Arc::clone(&mock));

        let user_text = "x".repeat(250);
        let q = extract_search_query_via_caller(&ctx, &user_text).await;
        // 100-char cap on fallback so the downstream `web_search` tool gets a
        // bounded query string.
        assert_eq!(q.chars().count(), 100);
        assert!(q.chars().all(|c| c == 'x'));
    }

    #[tokio::test]
    async fn extract_search_query_falls_back_when_response_is_empty_after_trim() {
        // Whitespace-only / quote-only response → cleaned == "" → fallback path.
        let mock = Arc::new(MockLlmCaller::with_responses(vec![Ok(
            "  \"\"  ".to_string(),
        )]));
        let ctx = ctx_with_mock(Arc::clone(&mock));

        let q = extract_search_query_via_caller(&ctx, "what is rust").await;
        assert_eq!(q, "what is rust",
            "empty cleaned response must fall back to the raw user text");
    }

    #[tokio::test]
    async fn extract_search_query_uses_router_endpoint_not_coding() {
        // Pin: query extraction is a router-tier task.  Routing it through
        // the coding model would burn the main provider's quota for a
        // ≤ 8-word completion.
        let mock = Arc::new(MockLlmCaller::with_responses(vec![Ok("q".to_string())]));
        let ctx = ctx_with_mock(Arc::clone(&mock));

        extract_search_query_via_caller(&ctx, "anything").await;
        let captured = mock.captured.lock().unwrap();
        assert_eq!(captured[0].0, LlmKind::Router,
            "extract_search_query_via_caller must use the router endpoint");
    }

    // ── resolve_search_context short-circuit paths ─────────────────────────

    #[tokio::test]
    async fn resolve_search_context_skips_llm_when_direct_answer_request() {
        // direct_answer_request=true is the highest-priority short-circuit:
        // no should_search check, no LLM call.  Mock has zero responses; it
        // panics if any call_* method is hit.
        let mock = Arc::new(MockLlmCaller::with_responses(vec![]));
        let ctx = ctx_with_mock(Arc::clone(&mock));

        let request = ChatRequest {
            user_text: "what is rust async?".to_string(),
            ..Default::default()
        };

        let out = resolve_search_context(&request, &ctx, /* direct_answer_request */ true).await;
        assert!(out.is_none());
        assert_eq!(mock.call_count(), 0);
    }

    #[tokio::test]
    async fn resolve_search_context_skips_llm_when_should_search_returns_false() {
        // Plain greeting — `should_search` returns false → no LLM call,
        // returns None without invoking the mock.
        let mock = Arc::new(MockLlmCaller::with_responses(vec![]));
        let ctx = ctx_with_mock(Arc::clone(&mock));

        let request = ChatRequest {
            user_text: "hi".to_string(),
            ..Default::default()
        };

        let out = resolve_search_context(&request, &ctx, /* direct_answer_request */ false).await;
        assert!(out.is_none());
        assert_eq!(mock.call_count(), 0,
            "no LLM call when should_search rejects the query");
    }

    // ── format_search_results (pre-existing pure helper, no #263 LLM) ──────

    #[test]
    fn format_search_results_skips_entries_without_title() {
        let results = serde_json::json!([
            { "title": "ok", "snippet": "s", "url": "https://a" },
            { "snippet": "no title", "url": "https://b" },
            { "title": "", "snippet": "empty title", "url": "https://c" },
        ]);
        let out = format_search_results(&results).expect("first entry survives");
        assert!(out.contains("- ok: s (https://a)"));
        assert!(!out.contains("no title"));
        assert!(!out.contains("empty title"));
    }

    #[test]
    fn format_search_results_returns_none_when_all_entries_invalid() {
        let results = serde_json::json!([
            { "snippet": "no title", "url": "x" },
        ]);
        assert!(format_search_results(&results).is_none());
    }
}
