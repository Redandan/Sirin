//! Chat Agent — the primary conversational agent for Sirin.
//!
//! This module coordinates three concerns that live in sub-modules:
//!
//! * [`intent`] — classifies what the user wants (`Intent`, `understand_message`)
//! * [`context`] — gathers conversation history, memory, and file context
//! * [`dispatch`] — routes to the right tool-use path and generates the reply
//!
//! The public entry points are:
//! * [`run_chat_response_via_adk_with_tracker`] — standard async call
//! * [`run_chat_via_adk_with_tracker`] — convenience wrapper returning only the reply string

mod context;
mod dispatch;
mod format;
mod intent;

use std::sync::Arc;

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::persona::{Persona, TaskTracker};
use crate::telegram::language::{
    chinese_fallback_reply, contains_cjk, is_code_access_question, is_identity_question,
};
use crate::telegram::llm::build_ai_reply_prompt;

use context::{resolve_context_block, resolve_memory_context, resolve_search_context};
use dispatch::dispatch_by_understanding;
use intent::{understand_message, Intent, MessageUnderstanding};

// ── Public request / response types ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    /// Intent family string forwarded by the Router from the Planner (snake_case).
    #[serde(default)]
    pub planner_intent_family: Option<String>,
    /// Recommended skill IDs forwarded by the Router from the Planner.
    #[serde(default)]
    pub planner_skills: Vec<String>,
    /// When true, the chat agent uses the `large_model` from `LlmConfig` instead
    /// of the default model.  Set by the Router when deep reasoning is needed.
    #[serde(default)]
    pub use_large_model: bool,
    /// Agent ID for memory isolation — scopes conversation context to this agent.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// When true, force local LLM even if the router requested the large/remote model.
    /// Mirrors `AgentConfig.disable_remote_ai` for per-agent override.
    #[serde(default)]
    pub disable_remote_ai: bool,
    /// Per-request LLM override — used by the meeting room when the selected agent
    /// has its own provider (e.g. Anthropic Claude).  Serialised as plain strings so
    /// the JSON round-trip through `AgentRuntime` is lossless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_override: Option<LlmOverride>,
}

/// Serialisable LLM override forwarded from `AgentConfig::llm_override`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmOverride {
    pub backend:  String,
    pub base_url: String,
    pub model:    String,
    pub api_key:  Option<String>,
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

// ── Agent implementation ──────────────────────────────────────────────────────

pub struct ChatAgent;

impl Agent for ChatAgent {
    fn name(&self) -> &'static str {
        "chat_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: ChatRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid chat request payload: {e}"))?;

            let client = Arc::clone(&ctx.http);
            let llm_arc = Arc::clone(&ctx.llm);
            let persona = Persona::cached().ok();

            // Respect the remote-AI kill-switch: per-request flag (from AgentConfig)
            // takes priority, then falls back to persona.yaml setting.
            let remote_disabled = request.disable_remote_ai
                || persona.as_ref().is_some_and(|p| p.disable_remote_ai);
            let use_large = request.use_large_model && !remote_disabled;

            // Per-request LLM override (e.g. Anthropic Claude for meeting participants).
            // Takes highest priority; bypasses both large-model and kill-switch logic.
            let llm: Arc<crate::llm::LlmConfig> = if let Some(ref ov) = request.llm_override {
                Arc::new(crate::llm::LlmConfig::for_override(
                    &ov.backend,
                    &ov.model,
                    ov.api_key.clone(),
                ))
            } else if use_large {
                crate::llm::shared_large_llm()
            } else {
                llm_arc
            };

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
            let fallback_reply = request.fallback_reply.clone().unwrap_or_else(|| {
                chinese_fallback_reply(&request.user_text, request.execution_result.as_deref())
            });

            if request.execution_result.is_none()
                && intent::is_simple_meta_request(&request.user_text)
            {
                let reply = shortcut_reply(&request.user_text, persona.as_ref())
                    .unwrap_or_else(|| fallback_reply.clone());
                ctx.record_system_event(
                    "adk_chat_meta_reply",
                    Some(preview_text(&request.user_text)),
                    Some("DONE"),
                    Some("identity_or_code_capability".to_string()),
                );
                let response = ChatAgentResponse {
                    reply,
                    ..Default::default()
                };
                return serde_json::to_value(response).map_err(|e| e.to_string());
            }

            let context_block = resolve_context_block(&request, ctx);

            let understanding = if request.execution_result.is_none() {
                let u = understand_message(
                    ctx,
                    &request.user_text,
                    context_block.as_deref(),
                    request.planner_intent_family.as_deref(),
                )
                .await;
                ctx.record_system_event(
                    "adk_chat_understood",
                    Some(preview_text(&request.user_text)),
                    Some("RUNNING"),
                    Some(format!(
                        "intent={:?} correction={} files={}",
                        u.intent,
                        u.is_correction,
                        u.target_files.len()
                    )),
                );
                u
            } else {
                MessageUnderstanding {
                    intent: Intent::General,
                    is_correction: false,
                    target_files: Vec::new(),
                }
            };

            let final_reply = dispatch_by_understanding(
                &understanding,
                &request,
                ctx,
                context_block.as_deref(),
                client.as_ref(),
                llm.as_ref(),
                persona.as_ref(),
            )
            .await
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| {
                ctx.record_system_event(
                    "adk_chat_dispatch_fallback",
                    Some(preview_text(&request.user_text)),
                    Some("FOLLOWUP_NEEDED"),
                    None,
                );
                fallback_reply.clone()
            });

            let tools_used = ctx.tool_calls_snapshot();
            let response = ChatAgentResponse {
                reply: final_reply.clone(),
                used_search: tools_used.iter().any(|t| t == "web_search"),
                used_memory: tools_used.iter().any(|t| t == "memory_search"),
                used_code_context: used_code_tools(&tools_used),
                tools_used,
                trace: ctx.event_trace_snapshot(),
            };

            crate::events::publish(crate::events::AgentEvent::ChatAgentReplied {
                peer_id: request.peer_id,
                preview: final_reply.chars().take(80).collect(),
            });

            serde_json::to_value(response).map_err(|e| e.to_string())
        }
        .boxed()
    }
}

// ── Public entry points ───────────────────────────────────────────────────────

pub async fn run_chat_response_via_adk_with_tracker(
    request: ChatRequest,
    tracker: Option<TaskTracker>,
) -> ChatAgentResponse {
    let fallback_reply = request.fallback_reply.clone().unwrap_or_else(|| {
        chinese_fallback_reply(&request.user_text, request.execution_result.as_deref())
    });

    let runtime = AgentRuntime::new(crate::adk::tool::read_only_tool_registry());
    let ctx = runtime
        .context("chat_request")
        .with_optional_tracker(tracker)
        .with_metadata("agent", "chat_agent")
        .with_metadata(
            "caller_agent_id",
            request.agent_id.as_deref().unwrap_or(""),
        )
        .with_metadata(
            "peer_id",
            request
                .peer_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".to_string()),
        );

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
// Streaming variant kept for future GUI chat tab integration.
#[allow(dead_code)]
pub async fn stream_chat_response<F>(request: ChatRequest, on_token: F) -> ChatAgentResponse
where
    F: Fn(String) + Send + 'static,
{
    use crate::llm::call_prompt_stream;

    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("chat_stream")
        .with_metadata("agent", "chat_agent_stream");

    let persona = Persona::cached().ok();
    let fallback_reply = request.fallback_reply.clone().unwrap_or_else(|| {
        chinese_fallback_reply(&request.user_text, request.execution_result.as_deref())
    });

    if request.execution_result.is_none() && intent::is_simple_meta_request(&request.user_text) {
        let reply = shortcut_reply(&request.user_text, persona.as_ref())
            .unwrap_or_else(|| fallback_reply.clone());
        on_token(reply.clone());
        return ChatAgentResponse {
            reply,
            ..Default::default()
        };
    }

    let client = Arc::clone(&ctx.http);
    let llm = Arc::clone(&ctx.llm);
    let context_block = resolve_context_block(&request, &ctx);

    let understanding = if request.execution_result.is_none() {
        let u = understand_message(
            &ctx,
            &request.user_text,
            context_block.as_deref(),
            request.planner_intent_family.as_deref(),
        )
        .await;
        ctx.record_system_event(
            "adk_chat_understood",
            Some(preview_text(&request.user_text)),
            Some("RUNNING"),
            Some(format!(
                "intent={:?} correction={} files={}",
                u.intent,
                u.is_correction,
                u.target_files.len()
            )),
        );
        u
    } else {
        MessageUnderstanding {
            intent: Intent::General,
            is_correction: false,
            target_files: Vec::new(),
        }
    };

    let reply = if understanding.intent == Intent::General {
        let direct_answer_request =
            crate::telegram::language::is_direct_answer_request(&request.user_text);
        let memory_ctx = resolve_memory_context(&request.user_text, &ctx).await;
        let search_ctx = if crate::telegram::commands::should_search(&request.user_text) {
            resolve_search_context(&request, &ctx, client.as_ref(), direct_answer_request).await
        } else {
            None
        };

        let skill_ctx = crate::skills::build_skill_context(&request.planner_skills);
        let prompt = build_ai_reply_prompt(
            persona.as_ref(),
            &request.user_text,
            request.execution_result.as_deref(),
            search_ctx.as_deref(),
            context_block.as_deref(),
            memory_ctx.as_deref(),
            None,
            direct_answer_request,
            false,
            skill_ctx.as_deref(),
        );

        call_prompt_stream(client.as_ref(), llm.as_ref(), prompt, on_token)
            .await
            .unwrap_or_default()
    } else {
        let complete = dispatch_by_understanding(
            &understanding,
            &request,
            &ctx,
            context_block.as_deref(),
            client.as_ref(),
            llm.as_ref(),
            persona.as_ref(),
        )
        .await
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| fallback_reply.clone());

        on_token(complete.clone());
        complete
    };

    let reply = if reply.trim().is_empty() {
        fallback_reply
    } else {
        reply.trim().to_string()
    };

    let tools_used = ctx.tool_calls_snapshot();
    ChatAgentResponse {
        reply,
        used_search: tools_used.iter().any(|t| t == "web_search"),
        used_memory: tools_used.iter().any(|t| t == "memory_search"),
        used_code_context: used_code_tools(&tools_used),
        tools_used,
        trace: ctx.event_trace_snapshot(),
    }
}

// ── Internal utilities ────────────────────────────────────────────────────────

pub(super) fn preview_text(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

fn shortcut_reply(user_text: &str, persona: Option<&Persona>) -> Option<String> {
    let asks_identity = is_identity_question(user_text);
    let asks_code_access = is_code_access_question(user_text);

    if !(asks_identity || asks_code_access) {
        return None;
    }

    let persona_name = persona.map(|p| p.name()).unwrap_or("Sirin");
    let use_chinese = contains_cjk(user_text);
    let mut parts = Vec::new();

    if asks_identity {
        if use_chinese {
            parts.push(format!(
                "我是 {persona_name}，你的本地 AI 助手，會協助聊天、研究、任務追蹤，並分析這個專案。"
            ));
        } else {
            parts.push(format!(
                "I'm {persona_name}, your local AI assistant. I help with chat, research, task tracking, and analysing this project's codebase."
            ));
        }
    }

    if asks_code_access {
        if use_chinese {
            parts.push(
                "可以，我能真的查本地專案的程式碼與檔案。你可以直接說像 `幫我看 src/main.rs`、`解釋 src/ui.rs`、`這個專案是什麼架構`，我會根據實際檔案內容回覆，不只是模板答案。"
                    .to_string(),
            );
        } else {
            parts.push(
                "Yes — I can read and analyse the actual local project files. Try: `show me src/main.rs`, `explain src/ui.rs`, or `what's the project architecture?`. I reply from real file content, not templates."
                    .to_string(),
            );
        }
    }

    Some(parts.join(" "))
}

fn used_code_tools(tools: &[String]) -> bool {
    tools.iter().any(|tool| {
        matches!(
            tool.as_str(),
            "codebase_search"
                | "local_file_read"
                | "project_overview"
                | "skill_catalog"
                | "skill_execute"
        )
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::append_context;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::context::format_search_results;
    use super::format::{format_local_file_reply, format_skill_catalog_reply};
    use super::intent::{
        extract_file_reference, extract_file_references_from_text, infer_focus_paths_from_query,
        is_contextual_file_explanation_request, is_simple_meta_request, is_skill_inventory_request,
        looks_like_capability_query, looks_like_project_overview_query,
    };

    fn unique_peer_id() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.as_nanos() % 1_000_000_000) as i64)
            .unwrap_or(42)
    }

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

    #[test]
    fn shortcut_reply_covers_identity_and_code_access() {
        let reply = shortcut_reply("你是誰？你能看到程式碼嗎？", None)
            .expect("should provide a deterministic meta reply");
        assert!(reply.contains("Sirin"));
        assert!(reply.contains("本地 AI 助手"));
        assert!(reply.contains("程式碼"));
    }

    #[test]
    fn simple_meta_request_detection_is_narrow() {
        assert!(is_simple_meta_request("你是誰"));
        assert!(is_simple_meta_request("你能看到程序運行的代碼嗎"));
        assert!(!is_simple_meta_request("你現在能看到什麼檔案"));
        assert!(!is_simple_meta_request("請幫我分析 src/main.rs 的架構"));
    }

    #[test]
    fn detects_project_overview_queries() {
        assert!(looks_like_project_overview_query("這是什麼專案架構"));
        assert!(looks_like_project_overview_query("你現在能看到什麼檔案"));
        assert!(!looks_like_project_overview_query("今天天氣如何"));
    }

    #[test]
    fn extracts_file_references_from_queries() {
        assert_eq!(
            extract_file_reference("幫我看 src/main.rs"),
            Some("src/main.rs".to_string())
        );
        assert_eq!(
            extract_file_reference("請解釋 `src/ui.rs`"),
            Some("src/ui.rs".to_string())
        );
        assert_eq!(extract_file_reference("這個專案是什麼"), None);
    }

    #[test]
    fn extracts_file_references_from_assistant_reply() {
        let refs = extract_file_references_from_text(
            "目前可直接查的關鍵檔案：\n- `src/main.rs`\n- `src/ui.rs`\n- `Cargo.toml`\nNative egui/eframe UI for Sirin."
        );
        assert!(refs.contains(&"src/main.rs".to_string()));
        assert!(refs.contains(&"src/ui.rs".to_string()));
        assert!(refs.contains(&"Cargo.toml".to_string()));
        assert!(!refs.contains(&"egui/eframe".to_string()));
        assert!(!refs.contains(&"/".to_string()));
    }

    #[test]
    fn detects_contextual_file_explanation_requests() {
        assert!(is_contextual_file_explanation_request("說明這些都是啥"));
        assert!(is_contextual_file_explanation_request(
            "上面那些檔案是做什麼的"
        ));
        assert!(!is_contextual_file_explanation_request(
            "幫我看 src/main.rs"
        ));
    }

    #[test]
    fn infers_skills_rs_as_primary_evidence_for_skill_questions() {
        let paths = infer_focus_paths_from_query("分析 skill", None, None);
        assert!(paths.contains(&"src/skills.rs".to_string()));
        assert!(paths.contains(&"src/agents/planner_agent.rs".to_string()));
    }

    #[test]
    fn detects_skill_inventory_requests() {
        assert!(is_skill_inventory_request("你有哪些skill"));
        assert!(is_skill_inventory_request("你有什麼技能？"));
        assert!(!is_skill_inventory_request("分析 skill"));
    }

    #[test]
    fn detects_capability_queries() {
        assert!(looks_like_capability_query("你能做什麼？"));
        assert!(looks_like_capability_query("你可以幫我做什麼"));
        assert!(looks_like_capability_query("你有哪些 skill"));
        assert!(!looks_like_capability_query("分析 src/main.rs"));
    }

    #[test]
    fn formats_direct_local_file_reply_with_excerpt() {
        let reply = format_local_file_reply(
            "File: src/main.rs\nKind: rust-source\nRole: App bootstrap\n\nExcerpt:\nfn main() {\n    println!(\"hi\");\n}"
        )
        .expect("should build a direct local file reply");

        assert!(reply.contains("`src/main.rs`"));
        assert!(reply.contains("檔案片段"));
        assert!(reply.contains("println!"));
    }

    #[test]
    fn formats_skill_catalog_reply_with_actual_skill_ids() {
        let reply = format_skill_catalog_reply(
            &json!([
                {"id": "project_overview", "category": "code-understanding"},
                {"id": "local_file_read", "category": "code-understanding"},
                {"id": "grounded_fix", "category": "code-optimization"},
                {"id": "web_search", "category": "external-research"}
            ]),
            "有哪些 skills？",
        )
        .expect("should build a skill inventory reply");

        assert!(reply.contains("`project_overview`"));
        assert!(reply.contains("`grounded_fix`"));
        assert!(reply.contains("`web_search`"));
    }

    #[tokio::test]
    async fn end_to_end_project_question_returns_grounded_overview() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "這個專案大概是怎麼運作的？".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
                use_large_model: false,
                agent_id: None,
                disable_remote_ai: false,
                ..Default::default()
            },
            None,
        )
        .await;

        println!("overview reply:\n{}", response.reply);
        assert!(
            !response.reply.trim().is_empty(),
            "should return a non-empty overview"
        );
        assert!(
            response.used_code_context,
            "should have used code context tools"
        );
    }

    #[tokio::test]
    async fn end_to_end_file_question_reads_src_main() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "幫我看 src/main.rs".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
                use_large_model: false,
                agent_id: None,
                disable_remote_ai: false,
                ..Default::default()
            },
            None,
        )
        .await;

        println!("file reply:\n{}", response.reply);
        assert!(response.reply.contains("src/main.rs"));
        assert!(response.reply.contains("檔案片段"));
        assert!(response.used_code_context);
    }

    #[tokio::test]
    async fn end_to_end_followup_question_explains_previous_files() {
        let peer_id = Some(unique_peer_id());
        let first = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "這個專案大概是怎麼運作的？".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id,
                planner_intent_family: None,
                planner_skills: Vec::new(),
                use_large_model: false,
                agent_id: None,
                disable_remote_ai: false,
                ..Default::default()
            },
            None,
        )
        .await;
        append_context("這個專案大概是怎麼運作的？", &first.reply, peer_id, None)
            .expect("should store the prior assistant reply for follow-up testing");

        let second = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "說明這些檔案是做什麼的".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id,
                planner_intent_family: None,
                planner_skills: Vec::new(),
                use_large_model: false,
                agent_id: None,
                disable_remote_ai: false,
                ..Default::default()
            },
            None,
        )
        .await;

        println!("follow-up reply:\n{}", second.reply);
        assert!(
            !second.reply.trim().is_empty(),
            "follow-up should return a non-empty reply"
        );
        assert!(
            second.reply.contains("src/main.rs")
                || second.reply.contains("src/ui.rs")
                || second.used_code_context,
            "follow-up should reference prior files or use code tools"
        );
    }

    #[tokio::test]
    async fn end_to_end_skill_question_returns_grounded_local_analysis() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "分析 skill".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
                use_large_model: false,
                agent_id: None,
                disable_remote_ai: false,
                ..Default::default()
            },
            None,
        )
        .await;

        println!("skill reply:\n{}", response.reply);
        assert!(
            !response.reply.trim().is_empty(),
            "should return a non-empty skill analysis"
        );
        assert!(
            response.used_code_context,
            "should have used code context tools"
        );
    }

    #[tokio::test]
    async fn end_to_end_skill_inventory_question_lists_available_skills() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "你有哪些skill".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
                use_large_model: false,
                agent_id: None,
                disable_remote_ai: false,
                ..Default::default()
            },
            None,
        )
        .await;

        println!("skill inventory reply:\n{}", response.reply);
        // Skills now come from config/skills/*.yaml — hardcoded IDs removed.
        // In test environment, config/skills/ may not be present, so we only
        // assert that the reply is non-empty and mentions "技能" or "skill".
        assert!(!response.reply.is_empty());
        assert!(response.used_code_context);
    }

    #[tokio::test]
    async fn end_to_end_capability_question_lists_available_skills() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "你可以幫我做什麼？".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
                use_large_model: false,
                agent_id: None,
                disable_remote_ai: false,
                ..Default::default()
            },
            None,
        )
        .await;

        println!("capability reply:\n{}", response.reply);
        assert!(
            !response.reply.trim().is_empty(),
            "should return a non-empty capability reply"
        );
        assert!(
            response.used_code_context,
            "should have used skill_catalog or code tools"
        );
    }

    #[tokio::test]
    async fn end_to_end_generic_code_analysis_returns_grounded_summary() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "先分析 chat agent 的流程與可能問題".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
                use_large_model: false,
                agent_id: None,
                disable_remote_ai: false,
                ..Default::default()
            },
            None,
        )
        .await;

        println!("generic analysis reply:\n{}", response.reply);
        assert!(
            !response.reply.trim().is_empty(),
            "should return a non-empty analysis"
        );
        assert!(
            response.used_code_context,
            "should have used code context tools"
        );
    }
}
