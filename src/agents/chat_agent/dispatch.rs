//! Response dispatch and ReAct loop.
//!
//! [`dispatch_by_understanding`] routes to the appropriate tool-use path based
//! on the classified intent.  [`react_loop`] implements a lightweight
//! Reason-Act loop where the LLM can call tools via bracket tags before
//! producing a final answer.

use serde_json::{json, Value};

use crate::adk::AgentContext;
use crate::llm::{call_prompt, LlmConfig};
use crate::persona::Persona;
use crate::telegram::language::is_direct_answer_request;
use crate::telegram::llm::generate_ai_reply;

use super::context::{load_local_file_reports, resolve_memory_context, resolve_search_context};
use super::format::{
    code_fence_language, extract_excerpt_block, format_local_file_reply, format_skill_catalog_reply,
    summarize_file_report_line,
};
use super::intent::{
    extract_file_reference, infer_focus_paths_from_query, is_file_view_request,
    is_skill_inventory_request, Intent, MessageUnderstanding,
};
use super::ChatRequest;

// ── ReAct loop ────────────────────────────────────────────────────────────────

/// Maximum number of tool-call iterations before forcing a final answer.
const REACT_MAX_TURNS: usize = 3;

// ── Typed prompt args (Issue #256) ──────────────────────────────────────────
//
// Adding/renaming a field surfaces at every call site as a compile error
// instead of a silent `{var}` literal in the rendered prompt.  Snapshot
// tests in `tests` mod pin the rendered shape so an accidental whitespace
// drift fails CI.

pub(super) struct ChatReactPromptArgs<'a> {
    pub(super) persona_name:     &'a str,
    pub(super) user_text:        &'a str,
    pub(super) initial_context:  Option<&'a str>,
    pub(super) has_meeting_auth: bool,
}

impl<'a> ChatReactPromptArgs<'a> {
    pub(super) fn render(&self) -> String {
        let handoff_line = if self.has_meeting_auth {
            "  [HANDOFF] <to_agent>::<payload>  — pass confidential info to another agent\n"
        } else {
            ""
        };
        let tool_instructions = format!(
            "You are an AI assistant. You can use these tools before answering:\n\
              [SEARCH] <query>  — search the web\n\
              [MEMORY] <query>  — recall past knowledge\n\
              [CODE]   <query>  — search this project's source code\n\
            {handoff_line}\n\
            When you have enough information, reply with:\n\
              [ANSWER] <your response to the user>\n\
            \n\
            Use at most one tool per turn. If no tool is needed, go straight to [ANSWER].\n\
            These bracket tags are internal only; never mention them in the final user-facing reply.",
            handoff_line = handoff_line,
        );
        let context_block = self.initial_context
            .map(|c| format!("\nContext:\n{c}\n"))
            .unwrap_or_default();
        format!(
            "System: You are {persona_name}. {tool_instructions}{context_block}\n\nUser: {user_text}\n",
            persona_name = self.persona_name,
            tool_instructions = tool_instructions,
            context_block = context_block,
            user_text = self.user_text,
        )
    }
}

/// Run a ReAct loop: the LLM decides which tools to call, we execute them and
/// feed results back until it produces a `[ANSWER]` tag or `REACT_MAX_TURNS`
/// is reached.
///
/// Bracket protocol (works with most local models):
/// ```text
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
    has_meeting_auth: bool,
) -> String {
    // Issue #256 — typed prompt args for chat ReAct system prompt.
    let mut history = ChatReactPromptArgs {
        persona_name,
        user_text,
        initial_context,
        has_meeting_auth,
    }.render();

    for _turn in 0..REACT_MAX_TURNS {
        let raw = match call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), history.clone()).await {
            Ok(r) => r,
            Err(_) => break,
        };

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
            } else if let Some(rest) = trimmed.strip_prefix("[HANDOFF]") {
                tool_name = Some("confidential_handoff");
                tool_query = rest.trim().to_string();
                break;
            } else if let Some(ans) = trimmed.strip_prefix("[ANSWER]") {
                final_answer = Some(ans.trim().to_string());
                break;
            }
        }

        if let Some(ans) = final_answer {
            ctx.record_system_event(
                "adk_react_final_answer",
                Some(user_text.chars().take(60).collect()),
                Some("DONE"),
                None,
            );
            return ans;
        }

        let Some(tool) = tool_name else {
            ctx.record_system_event(
                "adk_react_untagged_answer",
                Some(user_text.chars().take(60).collect()),
                Some("DONE"),
                None,
            );
            return raw;
        };

        ctx.record_system_event(
            "adk_react_tool_call",
            Some(user_text.chars().take(60).collect()),
            Some("RUNNING"),
            Some(format!(
                "tool={tool} query={}",
                &tool_query.chars().take(60).collect::<String>()
            )),
        );

        let call_result = if tool == "confidential_handoff" {
            // [HANDOFF] format: "to_agent::payload"
            let (to, payload) = tool_query
                .split_once("::")
                .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
                .unwrap_or_else(|| (String::new(), tool_query.clone()));
            ctx.call_tool("confidential_handoff", serde_json::json!({ "to_agent": to, "payload": payload }))
                .await
        } else {
            ctx.call_tool(tool, serde_json::json!({ "query": tool_query, "limit": 3 }))
                .await
        };
        let tool_result = call_result.unwrap_or_else(|_| serde_json::Value::String("(no results)".into()));

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

        history.push_str(&format!(
            "\nAssistant (tool call): [{tool_name_upper}] {tool_query}\nTool result:\n{result_text}\n",
            tool_name_upper = tool.to_uppercase(),
        ));
    }

    let final_prompt = format!(
        "{history}\nSystem: You have used all available tool turns. \
         Provide a final [ANSWER] now based on what you know.\n"
    );
    let final_raw = call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), final_prompt)
        .await
        .unwrap_or_default();
    final_raw
        .lines()
        .find(|l| l.trim().starts_with("[ANSWER]"))
        .and_then(|l| {
            l.trim()
                .strip_prefix("[ANSWER]")
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| final_raw.trim().to_string())
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

/// Core routing function: given the LLM's understanding of the message, call
/// the appropriate tools and produce a reply string.  Returns `None` when all
/// paths fail so the caller can substitute the fallback reply.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_by_understanding(
    understanding: &MessageUnderstanding,
    request: &ChatRequest,
    ctx: &AgentContext,
    context_block: Option<&str>,
    client: &reqwest::Client,
    llm: &LlmConfig,
    persona: Option<&Persona>,
) -> Option<String> {
    let persona_name = persona.map(|p| p.name()).unwrap_or("Sirin");
    let direct_answer = is_direct_answer_request(&request.user_text);
    // Check if this agent has meeting-room handoff auth (for [HANDOFF] bracket tag).
    let has_mtg_auth = ctx.metadata.get("caller_agent_id")
        .map(|id| crate::meeting::with_session(|s| {
            s.map(|sess| sess.auths.iter().any(|a| &a.from_agent == id))
             .unwrap_or(false)
        }))
        .unwrap_or(false);
    let skill_ctx = crate::skills::build_skill_context(&request.planner_skills);

    match &understanding.intent {
        // ── Read a specific local file ────────────────────────────────────────
        Intent::LocalFile => {
            let path = understanding
                .target_files
                .first()
                .cloned()
                .or_else(|| extract_file_reference(&request.user_text))?;

            match ctx
                .call_tool(
                    "local_file_read",
                    json!({ "path": path, "max_chars": 2200 }),
                )
                .await
            {
                Ok(result) => {
                    let content = result.get("content").and_then(Value::as_str)?;

                    if is_file_view_request(&request.user_text) {
                        let reply = format_local_file_reply(content)?;
                        ctx.record_system_event(
                            "adk_chat_direct_file_reply",
                            Some(super::preview_text(&request.user_text)),
                            Some("DONE"),
                            Some(format!("path={path}")),
                        );
                        Some(reply)
                    } else {
                        let excerpt = extract_excerpt_block(content).unwrap_or(content);
                        let fence = code_fence_language(&path);
                        let code_ctx = format!("Contents of `{path}`:\n```{fence}\n{excerpt}\n```");
                        ctx.record_system_event(
                            "adk_chat_file_question_llm",
                            Some(super::preview_text(&request.user_text)),
                            Some("RUNNING"),
                            Some(format!("path={path}")),
                        );
                        generate_ai_reply(
                            client,
                            llm,
                            persona,
                            &request.user_text,
                            None,
                            None,
                            context_block,
                            None,
                            Some(&code_ctx),
                            direct_answer,
                            false,
                            skill_ctx.as_deref(),
                        )
                        .await
                        .ok()
                        .or_else(|| format_local_file_reply(content))
                    }
                }
                Err(err) => {
                    ctx.record_system_event(
                        "adk_chat_direct_file_reply_error",
                        Some(super::preview_text(&request.user_text)),
                        Some("FOLLOWUP_NEEDED"),
                        Some(err),
                    );
                    None
                }
            }
        }

        // ── User says the previous reply was wrong — re-examine ───────────────
        Intent::Correction => {
            let correction_ctx = context_block.map(|c| {
                format!(
                    "IMPORTANT: The user says the previous reply was inaccurate or outdated. \
                     Re-examine the current state of the codebase and provide a correct, \
                     up-to-date answer.\n\nPrevious conversation:\n{c}"
                )
            });
            ctx.record_system_event(
                "adk_chat_correction_react",
                Some(super::preview_text(&request.user_text)),
                Some("RUNNING"),
                None,
            );
            let reply = react_loop(
                ctx,
                &request.user_text,
                persona_name,
                correction_ctx.as_deref().or(context_block),
                has_mtg_auth,
            )
            .await;
            if reply.trim().is_empty() {
                None
            } else {
                Some(reply)
            }
        }

        // ── Project structure / module overview ───────────────────────────────
        Intent::ProjectOverview => {
            let result = ctx
                .call_tool("project_overview", json!({ "limit": 8 }))
                .await
                .ok()?;
            let summary = result
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let files: Vec<String> = result
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();

            let reports = load_local_file_reports(ctx, &request.user_text, &files, 4, 1200).await;
            ctx.record_system_event(
                "adk_chat_project_overview_loaded",
                Some(super::preview_text(&request.user_text)),
                Some("RUNNING"),
                Some(format!(
                    "files={}, inspected={}",
                    files.len(),
                    reports.len()
                )),
            );

            let mut code_ctx = String::new();
            if !summary.is_empty() {
                code_ctx.push_str("Project summary: ");
                code_ctx.push_str(summary);
                code_ctx.push_str("\n\n");
            }
            if !reports.is_empty() {
                code_ctx.push_str("Key files inspected:\n");
                for report in &reports {
                    if let Some(line) = summarize_file_report_line(report) {
                        code_ctx.push_str(&line);
                        code_ctx.push('\n');
                    }
                }
            }
            if !files.is_empty() {
                code_ctx.push_str("\nAll discovered files:\n");
                for f in files.iter().take(8) {
                    code_ctx.push_str(&format!("- {f}\n"));
                }
            }

            let memory_ctx = resolve_memory_context(&request.user_text, ctx).await;
            generate_ai_reply(
                client,
                llm,
                persona,
                &request.user_text,
                None,
                None,
                context_block,
                memory_ctx.as_deref(),
                Some(&code_ctx),
                direct_answer,
                false,
                skill_ctx.as_deref(),
            )
            .await
            .ok()
        }

        // ── Code analysis — load files then reason with ReAct ─────────────────
        Intent::CodeAnalysis => {
            let paths = if !understanding.target_files.is_empty() {
                understanding.target_files.clone()
            } else {
                infer_focus_paths_from_query(
                    &request.user_text,
                    request.peer_id,
                    request.planner_intent_family.as_deref(),
                )
            };

            let reports = load_local_file_reports(ctx, &request.user_text, &paths, 4, 1600).await;

            let memory_ctx = resolve_memory_context(&request.user_text, ctx).await;
            let combined_ctx = {
                let mut parts: Vec<&str> = Vec::new();
                let grounded;
                if !reports.is_empty() {
                    grounded = format!("Grounded local evidence:\n{}", reports.join("\n\n===\n\n"));
                    parts.push(&grounded);
                }
                if let Some(c) = context_block {
                    parts.push(c);
                }
                if let Some(ref m) = memory_ctx {
                    parts.push(m);
                }
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join("\n\n"))
                }
            };

            ctx.record_system_event(
                "adk_chat_code_analysis_react",
                Some(super::preview_text(&request.user_text)),
                Some("RUNNING"),
                Some(format!("files={}", reports.len())),
            );

            let reply = react_loop(
                ctx,
                &request.user_text,
                persona_name,
                combined_ctx.as_deref(),
                has_mtg_auth,
            )
            .await;
            if reply.trim().is_empty() {
                None
            } else {
                Some(reply)
            }
        }

        // ── What skills / capabilities does the agent have? ───────────────────
        Intent::CapabilityQuery => {
            match ctx
                .call_tool("skill_catalog", json!({ "query": request.user_text }))
                .await
            {
                Ok(catalog) => {
                    let count = catalog.as_array().map(|v| v.len()).unwrap_or(0);
                    ctx.record_system_event(
                        "adk_chat_skill_catalog_reply",
                        Some(super::preview_text(&request.user_text)),
                        Some("DONE"),
                        Some(format!("count={count}")),
                    );

                    if is_skill_inventory_request(&request.user_text) {
                        format_skill_catalog_reply(&catalog, &request.user_text)
                    } else {
                        let skill_lines: Vec<String> = catalog
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|s| {
                                let id = s.get("id").and_then(|v| v.as_str())?;
                                let desc =
                                    s.get("description").and_then(|v| v.as_str()).unwrap_or("");
                                Some(format!("- {id}: {desc}"))
                            })
                            .collect();
                        let catalog_ctx = format!(
                            "Available skills and capabilities:\n{}",
                            skill_lines.join("\n")
                        );
                        generate_ai_reply(
                            client,
                            llm,
                            persona,
                            &request.user_text,
                            None,
                            None,
                            context_block,
                            None,
                            Some(&catalog_ctx),
                            direct_answer,
                            false,
                            skill_ctx.as_deref(),
                        )
                        .await
                        .ok()
                        .or_else(|| format_skill_catalog_reply(&catalog, &request.user_text))
                    }
                }
                Err(err) => {
                    ctx.record_system_event(
                        "adk_chat_skill_catalog_error",
                        Some(super::preview_text(&request.user_text)),
                        Some("FOLLOWUP_NEEDED"),
                        Some(err),
                    );
                    None
                }
            }
        }

        // ── Web / external information — use ReAct search loop ────────────────
        Intent::WebSearch => {
            ctx.record_system_event(
                "adk_chat_web_search_react",
                Some(super::preview_text(&request.user_text)),
                Some("RUNNING"),
                None,
            );
            if request.planner_intent_family.as_deref() == Some("research") {
                crate::events::publish(crate::events::AgentEvent::ResearchRequested {
                    topic: request.user_text.clone(),
                    url: None,
                });
            }
            let reply = react_loop(ctx, &request.user_text, persona_name, context_block, has_mtg_auth).await;
            if reply.trim().is_empty() {
                None
            } else {
                Some(reply)
            }
        }

        // ── General conversation — linear LLM call with memory context ────────
        Intent::General => {
            // If the agent is in a meeting (with or without handoff auth), route
            // through react_loop so tools are available.  Pass has_mtg_auth so
            // react_loop only shows [HANDOFF] when the agent is actually authorised.
            let in_meeting = context_block
                .map(|c| c.contains("[Meeting]"))
                .unwrap_or(false);
            if has_mtg_auth || in_meeting {
                ctx.record_system_event(
                    "adk_chat_meeting_react",
                    Some(super::preview_text(&request.user_text)),
                    Some("RUNNING"),
                    None,
                );
                let reply = react_loop(ctx, &request.user_text, persona_name, context_block, has_mtg_auth).await;
                return if reply.trim().is_empty() { None } else { Some(reply) };
            }

            let memory_ctx = resolve_memory_context(&request.user_text, ctx).await;
            let search_ctx = if crate::telegram::commands::should_search(&request.user_text) {
                resolve_search_context(request, ctx, client, direct_answer).await
            } else {
                None
            };

            generate_ai_reply(
                client,
                llm,
                persona,
                &request.user_text,
                request.execution_result.as_deref(),
                search_ctx.as_deref(),
                context_block,
                memory_ctx.as_deref(),
                None,
                direct_answer,
                false,
                skill_ctx.as_deref(),
            )
            .await
            .ok()
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// Issue #256 — snapshot tests for the typed prompt args.

#[cfg(test)]
mod prompt_tests {
    use super::ChatReactPromptArgs;

    #[test]
    fn chat_react_prompt_includes_all_fields() {
        let rendered = ChatReactPromptArgs {
            persona_name:     "Sirin",
            user_text:        "你好",
            initial_context:  Some("earlier session note"),
            has_meeting_auth: false,
        }.render();
        assert!(rendered.contains("System: You are Sirin"));
        assert!(rendered.contains("User: 你好"));
        assert!(rendered.contains("[SEARCH]"));
        assert!(rendered.contains("[ANSWER]"));
        assert!(rendered.contains("earlier session note"));
        // No HANDOFF without meeting auth.
        assert!(!rendered.contains("[HANDOFF]"));
        // No leaked {var} markers.
        assert!(!rendered.contains("{persona_name}"));
        assert!(!rendered.contains("{user_text}"));
    }

    #[test]
    fn chat_react_prompt_handoff_only_with_meeting_auth() {
        let with_auth = ChatReactPromptArgs {
            persona_name: "Sirin",
            user_text:    "hi",
            initial_context: None,
            has_meeting_auth: true,
        }.render();
        assert!(with_auth.contains("[HANDOFF]"));
    }

    #[test]
    fn chat_react_prompt_omits_context_section_when_none() {
        let rendered = ChatReactPromptArgs {
            persona_name: "Sirin",
            user_text:    "hi",
            initial_context: None,
            has_meeting_auth: false,
        }.render();
        assert!(!rendered.contains("Context:"));
    }
}
