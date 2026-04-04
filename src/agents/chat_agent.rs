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
use crate::telegram::llm::generate_ai_reply;

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

            let client = reqwest::Client::new();
            let llm = LlmConfig::from_env();
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
            let search_context = resolve_search_context(&request, ctx, &client, &llm, direct_answer_request).await;
            let memory_context = resolve_memory_context(&request.user_text, ctx).await;
            let code_context = resolve_code_context(&request.user_text, ctx).await;

            let ai_reply = match generate_ai_reply(
                &client,
                &llm,
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
            };

            let final_reply = if contains_cjk(&request.user_text)
                && (!contains_cjk(&ai_reply) || is_mixed_language_reply(&ai_reply))
            {
                match generate_ai_reply(
                    &client,
                    &llm,
                    persona.as_ref(),
                    &request.user_text,
                    request.execution_result.as_deref(),
                    search_context.as_deref(),
                    context_block.as_deref(),
                    memory_context.as_deref(),
                    code_context.as_deref(),
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
                used_search: search_context.is_some(),
                used_memory: memory_context.is_some(),
                used_code_context: code_context.is_some(),
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
