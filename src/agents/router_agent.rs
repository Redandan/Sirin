use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::persona::TaskTracker;
use crate::telegram::commands::detect_research_intent;
use crate::telegram::language::{
    is_code_access_question, is_direct_answer_request, is_identity_question,
};

use super::{chat_agent::ChatRequest, planner_agent::{PlanIntent, PlannerRequest}, research_agent::ResearchRequest};

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Chat,
    Research,
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
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: RouterRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid router request payload: {e}"))?;

            let plan = super::planner_agent::run_planner_via_adk(
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
            .ok();

            let route = if is_identity_question(&request.user_text)
                || is_code_access_question(&request.user_text)
                || is_direct_answer_request(&request.user_text)
            {
                RouteTarget::Chat
            } else if plan
                .as_ref()
                .map(|p| should_prefer_chat_from_skills(&p.recommended_skills))
                .unwrap_or(false)
            {
                RouteTarget::Chat
            } else {
                match plan.as_ref().map(|p| &p.intent) {
                    Some(PlanIntent::Research) => RouteTarget::Research,
                    _ => classify_route(&request.user_text),
                }
            };
            let route_name = match route {
                RouteTarget::Chat => "chat",
                RouteTarget::Research => "research",
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

            match route {
                RouteTarget::Research => {
                    let (topic, url) = detect_research_intent(&request.user_text)
                        .unwrap_or((request.user_text.clone(), None));
                    Ok(json!({
                        "route": route,
                        "planner_summary": plan_summary,
                        "recommended_skills": plan.as_ref().map(|p| p.recommended_skills.clone()).unwrap_or_default(),
                        "chat_request": {
                            "user_text": request.user_text,
                            "execution_result": Some(format!(
                                "執行結果：已啟動背景調研任務「{}{}」，完成後結果將記錄在任務板。",
                                topic,
                                url.as_ref().map(|v| format!(" ({v})")).unwrap_or_default()
                            )),
                            "context_block": request.context_block,
                            "fallback_reply": request.fallback_reply,
                            "peer_id": request.peer_id,
                        },
                        "research_request": ResearchRequest { topic, url }
                    }))
                }
                RouteTarget::Chat => Ok(json!({
                    "route": route,
                    "planner_summary": plan_summary,
                    "recommended_skills": plan.as_ref().map(|p| p.recommended_skills.clone()).unwrap_or_default(),
                    "chat_request": ChatRequest {
                        user_text: request.user_text,
                        execution_result: request.execution_result,
                        context_block: request.context_block,
                        fallback_reply: request.fallback_reply,
                        peer_id: request.peer_id,
                    }
                })),
            }
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

pub fn classify_route(text: &str) -> RouteTarget {
    if is_identity_question(text) || is_code_access_question(text) || is_direct_answer_request(text)
    {
        RouteTarget::Chat
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
        assert_eq!(classify_route("幫我研究 Rust async runtime"), RouteTarget::Research);
        assert_eq!(classify_route("直接講重點，不要貼連結"), RouteTarget::Chat);
        assert_eq!(classify_route("你是誰"), RouteTarget::Chat);
        assert_eq!(classify_route("能看到當前代碼嗎"), RouteTarget::Chat);
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
            },
            None,
        )
        .await
        .expect("router should succeed");

        assert_eq!(output.get("route").and_then(Value::as_str), Some("chat"));
        let recommended = output
            .get("recommended_skills")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(recommended.iter().any(|skill| skill.as_str() == Some("code_change_planning")));
        assert!(recommended.iter().any(|skill| skill.as_str() == Some("grounded_fix")));
    }
}
