use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::persona::TaskTracker;
use crate::telegram::commands::detect_research_intent;
use crate::telegram::language::{
    is_code_access_question, is_direct_answer_request, is_identity_question,
};

use super::{
    chat_agent::ChatRequest,
    planner_agent::{IntentFamily, PlanIntent, PlannerRequest},
    research_agent::ResearchRequest,
};


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterRequest {
    pub user_text: String,
    #[serde(default)]
    pub context_block: Option<String>,
    #[serde(default)]
    pub peer_id: Option<i64>,
    #[serde(default)]
    pub fallback_reply: Option<String>,
    #[serde(default)]
    pub execution_result: Option<String>,
    /// Agent ID for memory isolation — forwarded into ChatRequest and used for
    /// context log path selection.
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Chat,
    Research,
    /// Route to ChatAgent but with the large/powerful model.
    /// Used when the planner detects deep reasoning, coding, or multi-step analysis.
    LargeModel,
}

pub struct RouterAgent;

impl Agent for RouterAgent {
    fn name(&self) -> &'static str {
        "router_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: RouterRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid router request payload: {e}"))?;

            // Fast path: identity / capability / direct-answer questions never
            // need the Planner — skip the LLM call entirely.
            let is_shortcut = is_identity_question(&request.user_text)
                || is_code_access_question(&request.user_text)
                || is_direct_answer_request(&request.user_text);

            let plan = if is_shortcut {
                None
            } else {
                super::planner_agent::run_planner_via_adk(
                    PlannerRequest {
                        user_text: request.user_text.clone(),
                        context_block: request.context_block.clone(),
                        peer_id: request.peer_id,
                        fallback_reply: request.fallback_reply.clone(),
                        execution_result: request.execution_result.clone(),
                    },
                    ctx.tracker().cloned(),
                )
                .await
                .ok()
            };

            let route = if is_shortcut {
                RouteTarget::Chat
            } else if let Some(plan) = plan.as_ref() {
                // Planner (LLM) result takes priority — respect semantic intent.
                // Keyword matching only acts as a fallback when Planner is absent.
                route_target_from_plan(plan, &request.user_text)
            } else if is_coding_request(&request.user_text) {
                // No planner result → fall back to keyword heuristic.
                RouteTarget::LargeModel
            } else {
                classify_route(&request.user_text)
            };

            let route_name = match route {
                RouteTarget::Chat => "chat",
                RouteTarget::Research => "research",
                RouteTarget::LargeModel => "large_model",
            };
            let plan_summary = plan
                .as_ref()
                .map(|p| p.summary.clone())
                .unwrap_or_else(|| "planner unavailable; using heuristic route".to_string());
            let skill_summary = plan
                .as_ref()
                .map(|p| p.recommended_skills.join(","))
                .unwrap_or_default();
            ctx.record_system_event(
                "adk_router_route_selected",
                Some(preview_text(&request.user_text)),
                Some("RUNNING"),
                Some(format!("route={route_name}; {plan_summary}; skills={skill_summary}")),
            );

            let planner_family_str = plan
                .as_ref()
                .map(|p| intent_family_as_str(&p.intent_family).to_string());
            let planner_skills = plan
                .as_ref()
                .map(|p| p.recommended_skills.clone())
                .unwrap_or_default();

            match route {
                RouteTarget::Research => {
                    let (topic, url) = detect_research_intent(&request.user_text)
                        .unwrap_or((request.user_text.clone(), None));
                    Ok(json!({
                        "route": route,
                        "planner_summary": plan_summary,
                        "intent_family": plan.as_ref().map(|p| p.intent_family.clone()).unwrap_or(IntentFamily::GeneralChat),
                        "recommended_skills": &planner_skills,
                        "chat_request": {
                            "user_text": request.user_text,
                            "execution_result": Some(format!(
                                "Background research task launched: \"{}{}\". \
                                 Results will be recorded in the task board upon completion.",
                                topic,
                                url.as_ref().map(|v| format!(" ({})", v)).unwrap_or_default()
                            )),
                            "context_block": request.context_block,
                            "fallback_reply": request.fallback_reply,
                            "peer_id": request.peer_id,
                            "planner_intent_family": planner_family_str,
                            "planner_skills": &planner_skills,
                            "agent_id": request.agent_id,
                        },
                        "research_request": ResearchRequest { topic, url }
                    }))
                }
                RouteTarget::Chat => Ok(json!({
                    "route": route,
                    "planner_summary": plan_summary,
                    "intent_family": plan.as_ref().map(|p| p.intent_family.clone()).unwrap_or(IntentFamily::GeneralChat),
                    "recommended_skills": &planner_skills,
                    "chat_request": ChatRequest {
                        user_text: request.user_text,
                        execution_result: request.execution_result,
                        context_block: request.context_block,
                        fallback_reply: request.fallback_reply,
                        peer_id: request.peer_id,
                        planner_intent_family: planner_family_str,
                        planner_skills,
                        use_large_model: false,
                        agent_id: request.agent_id,
                        disable_remote_ai: false,
                        llm_override: None,
                    }
                })),
                RouteTarget::LargeModel => Ok(json!({
                    "route": route,
                    "planner_summary": plan_summary,
                    "intent_family": plan.as_ref().map(|p| p.intent_family.clone()).unwrap_or(IntentFamily::GeneralChat),
                    "recommended_skills": &planner_skills,
                    "chat_request": ChatRequest {
                        user_text: request.user_text,
                        execution_result: request.execution_result,
                        context_block: request.context_block,
                        fallback_reply: request.fallback_reply,
                        peer_id: request.peer_id,
                        planner_intent_family: planner_family_str,
                        planner_skills,
                        use_large_model: true,
                        agent_id: request.agent_id,
                        disable_remote_ai: false,
                        llm_override: None,
                    }
                })),
            }
        }
        .boxed()
    }
}

fn intent_family_as_str(family: &IntentFamily) -> &'static str {
    match family {
        IntentFamily::Capability => "capability",
        IntentFamily::LocalFile => "local_file",
        IntentFamily::ProjectOverview => "project_overview",
        IntentFamily::SkillArchitecture => "skill_architecture",
        IntentFamily::CodeAnalysis => "code_analysis",
        IntentFamily::Research => "research",
        IntentFamily::GeneralChat => "general_chat",
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

fn should_prefer_chat_from_skills(skills: &[String]) -> bool {
    skills.iter().any(|skill| {
        matches!(
            skill.as_str(),
            "project_overview"
                | "local_file_read"
                | "codebase_search"
                | "memory_search"
                | "code_change_planning"
                | "symbol_trace"
                | "grounded_fix"
                | "test_selector"
                | "architecture_consistency_check"
        )
    })
}

/// Skills that indicate the request benefits from the large/powerful model.
const LARGE_MODEL_SKILLS: &[&str] = &[
    "deep_reasoning",
    "multi_step_plan",
    "cross_module_analysis",
    "architecture_design",
    "security_audit",
];

/// Returns `true` when the planner recommends skills that benefit from a
/// large/powerful model (deep multi-step reasoning, cross-module analysis).
fn should_use_large_model_from_skills(skills: &[String]) -> bool {
    skills
        .iter()
        .any(|skill| LARGE_MODEL_SKILLS.contains(&skill.as_str()))
}

fn route_target_from_plan(plan: &super::planner_agent::WorkflowPlan, text: &str) -> RouteTarget {
    // Deep-reasoning skills always escalate to the large model regardless of family.
    if should_use_large_model_from_skills(&plan.recommended_skills) {
        return RouteTarget::LargeModel;
    }

    match plan.intent_family {
        IntentFamily::Research => RouteTarget::Research,
        // LocalFile / CodeAnalysis: could be read-only Q&A *or* a write task.
        // Use keyword check to distinguish "讀/解釋" from "改/重構/實作".
        IntentFamily::LocalFile | IntentFamily::CodeAnalysis => {
            if is_coding_request(text) {
                RouteTarget::LargeModel
            } else {
                RouteTarget::Chat
            }
        }
        IntentFamily::Capability
        | IntentFamily::ProjectOverview
        | IntentFamily::SkillArchitecture => RouteTarget::Chat,
        IntentFamily::GeneralChat => {
            if matches!(plan.intent, PlanIntent::Research)
                && !should_prefer_chat_from_skills(&plan.recommended_skills)
            {
                RouteTarget::Research
            } else {
                classify_route(text)
            }
        }
    }
}

/// Returns `true` when the user message expresses intent to modify or generate code.
pub fn is_coding_request(text: &str) -> bool {
    let lower = text.to_lowercase();
    let compact: String = lower.split_whitespace().collect();
    // Chinese coding keywords
    let zh_keywords = [
        "幫我寫",
        "帮我写",
        "幫我修",
        "帮我修",
        "幫我改",
        "帮我改",
        "幫我實作",
        "帮我实现",
        "幫我新增",
        "帮我添加",
        "修改代碼",
        "修改代码",
        "修改程式",
        "重構",
        "重构",
        "加功能",
        "加一個功能",
        "新增功能",
        "新增一個",
        "幫我重構",
        "帮我重构",
        "實作一個",
        "实现一个",
        "寫一個",
        "写一个",
        "coding",
        "實作",
        "实现",
    ];
    // English coding keywords
    let en_keywords = [
        "implement",
        "refactor",
        "fix the bug",
        "add a feature",
        "add feature",
        "create a function",
        "write a function",
        "modify the code",
        "update the code",
        "change the code",
        "edit the file",
        "write code",
        "generate code",
    ];
    zh_keywords.iter().any(|kw| compact.contains(kw))
        || en_keywords.iter().any(|kw| lower.contains(kw))
}

pub fn classify_route(text: &str) -> RouteTarget {
    if is_identity_question(text) || is_code_access_question(text) || is_direct_answer_request(text)
    {
        RouteTarget::Chat
    } else if is_coding_request(text) {
        RouteTarget::LargeModel
    } else if detect_research_intent(text).is_some() {
        RouteTarget::Research
    } else {
        RouteTarget::Chat
    }
}

pub async fn run_router_via_adk(
    request: RouterRequest,
    tracker: Option<TaskTracker>,
) -> Result<Value, String> {
    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("router_request")
        .with_optional_tracker(tracker)
        .with_metadata("agent", "router_agent");
    runtime.run(&RouterAgent, ctx, json!(request)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_research_requests() {
        assert_eq!(
            classify_route("幫我研究 Rust async runtime"),
            RouteTarget::Research
        );
        assert_eq!(classify_route("直接講重點，不要貼連結"), RouteTarget::Chat);
        assert_eq!(classify_route("你是誰"), RouteTarget::Chat);
        assert_eq!(classify_route("能看到當前代碼嗎"), RouteTarget::Chat);
    }

    #[test]
    fn classifies_coding_requests() {
        assert_eq!(
            classify_route("幫我修改 src/llm.rs 的 error handling"),
            RouteTarget::LargeModel
        );
        assert_eq!(classify_route("幫我重構 router_agent"), RouteTarget::LargeModel);
        assert_eq!(
            classify_route("implement a new feature"),
            RouteTarget::LargeModel
        );
        assert_eq!(
            classify_route("refactor the chat module"),
            RouteTarget::LargeModel
        );
    }

    #[test]
    fn prefers_chat_when_skill_hints_indicate_local_code_work() {
        let skills = vec![
            "code_change_planning".to_string(),
            "grounded_fix".to_string(),
            "test_selector".to_string(),
        ];
        assert!(should_prefer_chat_from_skills(&skills));
    }

    #[tokio::test]
    async fn router_exposes_recommended_skills_for_local_optimization_requests() {
        let output = run_router_via_adk(
            RouterRequest {
                user_text: "先分析再改，幫我安全優化這段 code 並跑測試".to_string(),
                context_block: None,
                peer_id: None,
                fallback_reply: None,
                execution_result: None,
                agent_id: None,
            },
            None,
        )
        .await
        .expect("router should succeed");

        assert_eq!(output.get("route").and_then(Value::as_str), Some("chat"));
        assert_eq!(
            output.get("intent_family").and_then(Value::as_str),
            Some("code_analysis")
        );
        // Hardcoded skills removed — recommended_skills now reflect YAML-defined skills only.
        // Assert the field exists and is an array; specific IDs are YAML-driven.
        assert!(output.get("recommended_skills").and_then(Value::as_array).is_some());
    }

    #[tokio::test]
    async fn router_exposes_project_overview_intent_family() {
        let output = run_router_via_adk(
            RouterRequest {
                user_text: "這個專案怎麼運作？".to_string(),
                context_block: None,
                peer_id: None,
                fallback_reply: None,
                execution_result: None,
                agent_id: None,
            },
            None,
        )
        .await
        .expect("router should succeed");

        assert_eq!(output.get("route").and_then(Value::as_str), Some("chat"));
        assert_eq!(
            output.get("intent_family").and_then(Value::as_str),
            Some("project_overview")
        );
    }

    #[tokio::test]
    async fn router_routes_coding_request_to_coding() {
        let output = run_router_via_adk(
            RouterRequest {
                user_text: "幫我重構 src/llm.rs 讓它支援更多後端".to_string(),
                context_block: None,
                peer_id: None,
                fallback_reply: None,
                execution_result: None,
                agent_id: None,
            },
            None,
        )
        .await
        .expect("router should succeed");

        assert_eq!(output.get("route").and_then(Value::as_str), Some("large_model"));
    }
}
