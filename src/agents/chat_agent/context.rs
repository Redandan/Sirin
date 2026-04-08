//! Context assembly for chat responses.
//!
//! Gathers conversation history, memory snippets, search results, and local
//! file contents so that the LLM has the right information to answer.

use serde_json::{json, Value};

use crate::adk::AgentContext;
use crate::memory::load_recent_context;
use crate::telegram::commands::{extract_search_query, should_search};

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
    client: &reqwest::Client,
    direct_answer_request: bool,
) -> Option<String> {
    if direct_answer_request || !should_search(&request.user_text) {
        return None;
    }

    let router_llm = crate::llm::shared_router_llm();
    let query = extract_search_query(client, &router_llm, &request.user_text).await;
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
