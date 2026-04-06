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
use crate::telegram::language::{contains_cjk, is_direct_answer_request};
use crate::telegram::llm::generate_ai_reply;

use super::ChatRequest;
use super::context::{load_local_file_reports, resolve_memory_context, resolve_search_context};
use super::intent::{
    extract_file_reference, infer_focus_paths_from_query, is_file_view_request,
    is_skill_inventory_request, Intent, MessageUnderstanding,
};

// ── ReAct loop ────────────────────────────────────────────────────────────────

/// Maximum number of tool-call iterations before forcing a final answer.
const REACT_MAX_TURNS: usize = 3;

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
) -> String {
    let tool_instructions = "\
You are an AI assistant. You can use these tools before answering:\n\
  [SEARCH] <query>  — search the web\n\
  [MEMORY] <query>  — recall past knowledge\n\
  [CODE]   <query>  — search this project's source code\n\
\n\
When you have enough information, reply with:\n\
  [ANSWER] <your response to the user>\n\
\n\
Use at most one tool per turn. If no tool is needed, go straight to [ANSWER].
These bracket tags are internal only; never mention them in the final user-facing reply.";

    let context_block = initial_context
        .map(|c| format!("\nContext:\n{c}\n"))
        .unwrap_or_default();

    let mut history = format!(
        "System: You are {persona_name}. {tool_instructions}{context_block}\n\nUser: {user_text}\n"
    );

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
        .and_then(|l| l.trim().strip_prefix("[ANSWER]").map(|s| s.trim().to_string()))
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

    match &understanding.intent {
        // ── Read a specific local file ────────────────────────────────────────
        Intent::LocalFile => {
            let path = understanding
                .target_files
                .first()
                .cloned()
                .or_else(|| extract_file_reference(&request.user_text))?;

            match ctx
                .call_tool("local_file_read", json!({ "path": path, "max_chars": 2200 }))
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
                        let code_ctx =
                            format!("Contents of `{path}`:\n```{fence}\n{excerpt}\n```");
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
            )
            .await;
            if reply.trim().is_empty() { None } else { Some(reply) }
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
                Some(format!("files={}, inspected={}", files.len(), reports.len())),
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
                if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
            };

            ctx.record_system_event(
                "adk_chat_code_analysis_react",
                Some(super::preview_text(&request.user_text)),
                Some("RUNNING"),
                Some(format!("files={}", reports.len())),
            );

            let reply = react_loop(ctx, &request.user_text, persona_name, combined_ctx.as_deref()).await;
            if reply.trim().is_empty() { None } else { Some(reply) }
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
                                let desc = s.get("description").and_then(|v| v.as_str()).unwrap_or("");
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
            let reply = react_loop(ctx, &request.user_text, persona_name, context_block).await;
            if reply.trim().is_empty() { None } else { Some(reply) }
        }

        // ── General conversation — linear LLM call with memory context ────────
        Intent::General => {
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
            )
            .await
            .ok()
        }
    }
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn extract_labeled_value<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
}

fn summarize_file_report_line(report: &str) -> Option<String> {
    let path = extract_labeled_value(report, "File: ")?;
    let role = extract_labeled_value(report, "Role: ").unwrap_or("No summary available");
    let kind = extract_labeled_value(report, "Kind: ").unwrap_or("text");
    Some(format!("- `{path}`：{role}（{kind}）"))
}

fn extract_excerpt_block(text: &str) -> Option<&str> {
    text.split_once("\nExcerpt:\n")
        .map(|(_, excerpt)| excerpt.trim())
        .filter(|excerpt| !excerpt.is_empty())
}

fn code_fence_language(path: &str) -> &'static str {
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
        lines.push(format!("- {}: {}", category_display_label(category), ids.join(sep)));
    }
    lines.push(footer);
    Some(lines.join("\n"))
}
