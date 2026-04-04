use std::sync::Arc;

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::llm::LlmConfig;
use crate::memory::{load_recent_context, looks_like_code_query};
use crate::persona::{Persona, TaskTracker};
use crate::researcher;
use crate::telegram::commands::{extract_search_query, should_search};
use crate::telegram::language::{
    chinese_fallback_reply, contains_cjk, is_direct_answer_request, is_mixed_language_reply,
};
use crate::telegram::llm::{build_ai_reply_prompt, generate_ai_reply};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub user_text: String,
    #[serde(default)]
    pub execution_result: Option<String>,
    #[serde(default)]
    pub context_block: Option<String>,
    #[serde(default)]
    pub fallback_reply: Option<String>,
    #[serde(default)]
    pub peer_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatAgentResponse {
    pub reply: String,
    #[serde(default)]
    pub used_search: bool,
    #[serde(default)]
    pub used_memory: bool,
    #[serde(default)]
    pub used_code_context: bool,
    #[serde(default)]
    pub tools_used: Vec<String>,
    #[serde(default)]
    pub trace: Vec<String>,
}

pub struct ChatAgent;

impl Agent for ChatAgent {
    fn name(&self) -> &'static str {
        "chat_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: ChatRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid chat request payload: {e}"))?;

            let client = Arc::clone(&ctx.http);
            let llm = Arc::clone(&ctx.llm);
            let persona = Persona::load().ok();
            let direct_answer_request = is_direct_answer_request(&request.user_text);

            let behavior = ctx
                .call_tool(
                    "behavior_evaluate",
                    json!({
                        "source": ctx.source,
                        "msg": request.user_text,
                        "estimated_value": 0.0,
                        "record": ctx.tracker().is_some()
                    }),
                )
                .await
                .unwrap_or_else(|_| json!({}));
            if let Some(tier) = behavior.get("tier").and_then(Value::as_str) {
                ctx.record_system_event(
                    "adk_chat_behavior_evaluated",
                    Some(preview_text(&request.user_text)),
                    Some("RUNNING"),
                    Some(format!("tier={tier}")),
                );
            }
            let fallback_reply = request
                .fallback_reply
                .clone()
                .unwrap_or_else(|| chinese_fallback_reply(&request.user_text, request.execution_result.as_deref()));

            let context_block = resolve_context_block(&request, ctx);

            // ── Route: ReAct loop for open-ended queries; linear path otherwise ──
            let use_react = !direct_answer_request
                && request.execution_result.is_none()
                && (should_search(&request.user_text)
                    || looks_like_code_query(&request.user_text));

            let ai_reply = if use_react {
                ctx.record_system_event(
                    "adk_chat_react_start",
                    Some(preview_text(&request.user_text)),
                    Some("RUNNING"),
                    None,
                );
                let persona_name = persona.as_ref().map(|p| p.name()).unwrap_or("Sirin");
                let reply = react_loop(ctx, &request.user_text, persona_name, context_block.as_deref()).await;
                if reply.trim().is_empty() { fallback_reply.clone() } else { reply }
            } else {
                // Linear path: deterministic context resolution + single LLM call.
                let search_context = resolve_search_context(&request, ctx, client.as_ref(), llm.as_ref(), direct_answer_request).await;
                let memory_context = resolve_memory_context(&request.user_text, ctx).await;
                let code_context = resolve_code_context(&request.user_text, ctx).await;

                match generate_ai_reply(
                    client.as_ref(),
                    llm.as_ref(),
                    persona.as_ref(),
                    &request.user_text,
                    request.execution_result.as_deref(),
                    search_context.as_deref(),
                    context_block.as_deref(),
                    memory_context.as_deref(),
                    code_context.as_deref(),
                    direct_answer_request,
                    false,
                )
                .await
                {
                    Ok(v) if !v.trim().is_empty() => v,
                    Ok(_) => fallback_reply.clone(),
                    Err(e) => {
                        ctx.record_system_event(
                            "adk_chat_llm_error",
                            Some(preview_text(&request.user_text)),
                            Some("FOLLOWUP_NEEDED"),
                            Some(e.to_string()),
                        );
                        fallback_reply.clone()
                    }
                }
            };

            let final_reply = if contains_cjk(&request.user_text)
                && (!contains_cjk(&ai_reply) || is_mixed_language_reply(&ai_reply))
            {
                match generate_ai_reply(
                    client.as_ref(),
                    llm.as_ref(),
                    persona.as_ref(),
                    &request.user_text,
                    request.execution_result.as_deref(),
                    None,
                    context_block.as_deref(),
                    None,
                    None,
                    direct_answer_request,
                    true,
                )
                .await
                {
                    Ok(v) if !v.trim().is_empty() && contains_cjk(&v) => v,
                    Ok(_) | Err(_) => chinese_fallback_reply(
                        &request.user_text,
                        request.execution_result.as_deref(),
                    ),
                }
            } else {
                ai_reply
            };

            let response = ChatAgentResponse {
                reply: final_reply,
                used_search: use_react || ctx.tool_calls_snapshot().iter().any(|t| t == "web_search"),
                used_memory: ctx.tool_calls_snapshot().iter().any(|t| t == "memory_search"),
                used_code_context: ctx.tool_calls_snapshot().iter().any(|t| t == "codebase_search"),
                tools_used: ctx.tool_calls_snapshot(),
                trace: ctx.event_trace_snapshot(),
            };

            serde_json::to_value(response).map_err(|e| e.to_string())
        }
        .boxed()
    }
}

fn preview_text(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

fn resolve_context_block(request: &ChatRequest, ctx: &AgentContext) -> Option<String> {
    if request.context_block.is_some() {
        return request.context_block.clone();
    }

    match load_recent_context(5, request.peer_id) {
        Ok(entries) if !entries.is_empty() => {
            ctx.record_system_event(
                "adk_chat_context_loaded",
                Some(preview_text(&request.user_text)),
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
                Some(preview_text(&request.user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err.to_string()),
            );
            None
        }
    }
}

async fn resolve_search_context(
    request: &ChatRequest,
    ctx: &AgentContext,
    client: &reqwest::Client,
    llm: &LlmConfig,
    direct_answer_request: bool,
) -> Option<String> {
    if direct_answer_request || !should_search(&request.user_text) {
        return None;
    }

    let query = extract_search_query(client, llm, &request.user_text).await;
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
                    Some(preview_text(&request.user_text)),
                    Some("RUNNING"),
                    Some(format!("query={query}, result_lines={result_count}")),
                );
            }
            formatted
        }
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_search_error",
                Some(preview_text(&request.user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err),
            );
            None
        }
    }
}

async fn resolve_memory_context(user_text: &str, ctx: &AgentContext) -> Option<String> {
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
                Some(preview_text(user_text)),
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
            Some(preview_text(user_text)),
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

async fn resolve_code_context(user_text: &str, ctx: &AgentContext) -> Option<String> {
    if !looks_like_code_query(user_text) {
        return None;
    }

    match ctx
        .call_tool("codebase_search", json!({ "query": user_text, "limit": 3 }))
        .await
    {
        Ok(results) => {
            let entries: Vec<String> = results
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|s| s.to_string())
                .collect();

            if entries.is_empty() {
                None
            } else {
                ctx.record_system_event(
                    "adk_chat_code_context_loaded",
                    Some(preview_text(user_text)),
                    Some("RUNNING"),
                    Some(format!("matches={}", entries.len())),
                );
                Some(entries.join("\n\n---\n\n"))
            }
        }
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_code_context_error",
                Some(preview_text(user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err),
            );
            None
        }
    }
}

// ── ReAct tool-use loop ──────────────────────────────���────────────────────────

/// Maximum number of tool-call iterations before forcing a final answer.
const REACT_MAX_TURNS: usize = 3;

/// Run a ReAct loop: the LLM decides which tools to call, we execute them and
/// feed results back until it produces a `[ANSWER]` tag or `REACT_MAX_TURNS`
/// is reached.
///
/// Bracket protocol (works with most local models):
/// ```
/// [SEARCH] query text        → calls web_search
/// [MEMORY] query text        → calls memory_search
/// [CODE]   query text        → calls codebase_search
/// [ANSWER] final reply text  → stops and returns the text
/// ```
/// Any response that contains none of the above tags is treated as a final answer.
pub async fn react_loop(
    ctx: &AgentContext,
    user_text: &str,
    persona_name: &str,
    initial_context: Option<&str>,
) -> String {
    use crate::llm::call_prompt;

    let tool_instructions = "\
You are an AI assistant. You can use these tools before answering:\n\
  [SEARCH] <query>  — search the web\n\
  [MEMORY] <query>  — recall past knowledge\n\
  [CODE]   <query>  — search this project's source code\n\
\n\
When you have enough information, reply with:\n\
  [ANSWER] <your response to the user>\n\
\n\
Use at most one tool per turn. If no tool is needed, go straight to [ANSWER].";

    let context_block = initial_context
        .map(|c| format!("\nContext:\n{c}\n"))
        .unwrap_or_default();

    // Conversation accumulated across iterations.
    let mut history = format!(
        "System: You are {persona_name}. {tool_instructions}{context_block}\n\nUser: {user_text}\n"
    );

    for _turn in 0..REACT_MAX_TURNS {
        let raw = match call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), history.clone()).await {
            Ok(r) => r,
            Err(_) => break,
        };

        // ── Parse the first recognised tag ─────────���─────────────────────────
        let mut tool_name: Option<&str> = None;
        let mut tool_query = String::new();
        let mut final_answer: Option<String> = None;

        for line in raw.lines() {
            let trimmed = line.trim();
            if let Some(q) = trimmed.strip_prefix("[SEARCH]") {
                tool_name = Some("web_search");
                tool_query = q.trim().to_string();
                break;
            } else if let Some(q) = trimmed.strip_prefix("[MEMORY]") {
                tool_name = Some("memory_search");
                tool_query = q.trim().to_string();
                break;
            } else if let Some(q) = trimmed.strip_prefix("[CODE]") {
                tool_name = Some("codebase_search");
                tool_query = q.trim().to_string();
                break;
            } else if let Some(ans) = trimmed.strip_prefix("[ANSWER]") {
                final_answer = Some(ans.trim().to_string());
                break;
            }
        }

        // ── Final answer ─────────────────────────��───────────────────────���────
        if let Some(ans) = final_answer {
            ctx.record_system_event(
                "adk_react_final_answer",
                Some(user_text.chars().take(60).collect()),
                Some("DONE"),
                None,
            );
            return ans;
        }

        // ── No recognised tag → treat whole response as answer ────────────────
        let Some(tool) = tool_name else {
            ctx.record_system_event(
                "adk_react_untagged_answer",
                Some(user_text.chars().take(60).collect()),
                Some("DONE"),
                None,
            );
            return raw;
        };

        // ── Execute tool ──────────────────────────────────────────────────────
        ctx.record_system_event(
            "adk_react_tool_call",
            Some(user_text.chars().take(60).collect()),
            Some("RUNNING"),
            Some(format!("tool={tool} query={}", &tool_query.chars().take(60).collect::<String>())),
        );

        let tool_result = ctx
            .call_tool(tool, serde_json::json!({ "query": tool_query, "limit": 3 }))
            .await
            .unwrap_or_else(|_| serde_json::Value::String("(no results)".into()));

        let result_text = match &tool_result {
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| {
                    if let serde_json::Value::Object(m) = v {
                        let title = m.get("title").and_then(|t| t.as_str()).unwrap_or("");
                        let snippet = m.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
                        Some(format!("- {title}: {snippet}"))
                    } else {
                        v.as_str().map(|s| format!("- {s}"))
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        // Append tool result to history and continue.
        history.push_str(&format!(
            "\nAssistant (tool call): [{tool_name_upper}] {tool_query}\nTool result:\n{result_text}\n",
            tool_name_upper = tool.to_uppercase(),
        ));
    }

    // Exhausted turns — ask for final answer without tools.
    let final_prompt = format!(
        "{history}\nSystem: You have used all available tool turns. \
         Provide a final [ANSWER] now based on what you know.\n"
    );
    call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), final_prompt)
        .await
        .unwrap_or_default()
        .lines()
        .find(|l| l.trim().starts_with("[ANSWER]"))
        .and_then(|l| l.trim().strip_prefix("[ANSWER]").map(|s| s.trim().to_string()))
        .unwrap_or_default()
}

fn related_research_snippet(user_text: &str) -> Option<String> {
    let lower_text = user_text.to_lowercase();
    researcher::list_research()
        .ok()?
        .into_iter()
        .filter(|task| task.status == researcher::ResearchStatus::Done)
        .filter(|task| {
            task.topic
                .to_lowercase()
                .split_whitespace()
                .filter(|word| word.len() > 2)
                .any(|word| lower_text.contains(word))
        })
        .filter_map(|task| task.final_report)
        .map(|report| report.chars().take(600).collect::<String>())
        .next()
}

fn format_search_results(results: &Value) -> Option<String> {
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
            let url = item.get("url").and_then(Value::as_str).unwrap_or_default().trim();

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

pub async fn run_chat_response_via_adk_with_tracker(
    request: ChatRequest,
    tracker: Option<TaskTracker>,
) -> ChatAgentResponse {
    let fallback_reply = request
        .fallback_reply
        .clone()
        .unwrap_or_else(|| chinese_fallback_reply(&request.user_text, request.execution_result.as_deref()));

    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("chat_request")
        .with_optional_tracker(tracker)
        .with_metadata("agent", "chat_agent")
        .with_metadata("peer_id", request.peer_id.map(|id| id.to_string()).unwrap_or_else(|| "none".to_string()));

    match runtime.run(&ChatAgent, ctx, json!(request)).await {
        Ok(output) => serde_json::from_value(output).unwrap_or_else(|_| ChatAgentResponse {
            reply: fallback_reply,
            ..Default::default()
        }),
        Err(_) => ChatAgentResponse {
            reply: fallback_reply,
            ..Default::default()
        },
    }
}

pub async fn run_chat_via_adk_with_tracker(
    request: ChatRequest,
    tracker: Option<TaskTracker>,
) -> String {
    run_chat_response_via_adk_with_tracker(request, tracker)
        .await
        .reply
}

/// Streaming variant for the GUI chat tab.
///
/// For the linear (non-ReAct) path, tokens are delivered to `on_token` as they
/// arrive so the caller can update the chat bubble progressively.
/// For ReAct queries the function runs the standard blocking pipeline and
/// returns the full reply without streaming (tokens are too interleaved with
/// tool calls to stream meaningfully).
pub async fn stream_chat_response<F>(request: ChatRequest, on_token: F) -> ChatAgentResponse
where
    F: Fn(String) + Send + 'static,
{
    use crate::llm::call_prompt_stream;

    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("chat_stream")
        .with_metadata("agent", "chat_agent_stream");

    let persona = Persona::load().ok();
    let direct_answer_request = is_direct_answer_request(&request.user_text);
    let use_react = !direct_answer_request
        && request.execution_result.is_none()
        && (should_search(&request.user_text) || looks_like_code_query(&request.user_text));

    if use_react {
        // ReAct path — interleaved tool calls; deliver complete reply at end.
        let persona_name = persona.as_ref().map(|p| p.name()).unwrap_or("Sirin");
        let context_block = resolve_context_block(&request, &ctx);
        let reply = react_loop(&ctx, &request.user_text, persona_name, context_block.as_deref()).await;
        let reply = if reply.trim().is_empty() {
            chinese_fallback_reply(&request.user_text, request.execution_result.as_deref())
        } else {
            reply
        };
        return ChatAgentResponse {
            reply,
            used_search: true,
            tools_used: ctx.tool_calls_snapshot(),
            trace: ctx.event_trace_snapshot(),
            ..Default::default()
        };
    }

    // Linear path — gather context, build prompt, stream tokens.
    let client = Arc::clone(&ctx.http);
    let llm = Arc::clone(&ctx.llm);
    let context_block = resolve_context_block(&request, &ctx);
    let search_ctx = resolve_search_context(
        &request,
        &ctx,
        client.as_ref(),
        llm.as_ref(),
        direct_answer_request,
    )
    .await;
    let memory_ctx = resolve_memory_context(&request.user_text, &ctx).await;
    let code_ctx = resolve_code_context(&request.user_text, &ctx).await;

    let prompt = build_ai_reply_prompt(
        persona.as_ref(),
        &request.user_text,
        request.execution_result.as_deref(),
        search_ctx.as_deref(),
        context_block.as_deref(),
        memory_ctx.as_deref(),
        code_ctx.as_deref(),
        direct_answer_request,
        false,
    );

    let reply = call_prompt_stream(client.as_ref(), llm.as_ref(), prompt, on_token)
        .await
        .unwrap_or_default();

    let reply = if reply.trim().is_empty() {
        chinese_fallback_reply(&request.user_text, request.execution_result.as_deref())
    } else {
        reply.trim().to_string()
    };

    ChatAgentResponse {
        reply,
        used_search: ctx.tool_calls_snapshot().iter().any(|t| t == "web_search"),
        used_memory: ctx.tool_calls_snapshot().iter().any(|t| t == "memory_search"),
        used_code_context: ctx.tool_calls_snapshot().iter().any(|t| t == "codebase_search"),
        tools_used: ctx.tool_calls_snapshot(),
        trace: ctx.event_trace_snapshot(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_search_results_into_prompt_block() {
        let value = json!([
            {
                "title": "Rust async",
                "snippet": "Tokio drives the runtime",
                "url": "https://example.com/rust"
            }
        ]);

        let formatted = format_search_results(&value).expect("should format results");
        assert!(formatted.contains("Rust async"));
        assert!(formatted.contains("Tokio drives the runtime"));
    }
}
