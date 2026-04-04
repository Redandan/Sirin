use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::persona::TaskTracker;
use crate::telegram::commands::detect_research_intent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerRequest {
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
pub enum PlanIntent {
    Answer,
    Research,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowPlan {
    pub intent: PlanIntent,
    pub steps: Vec<String>,
    #[serde(default)]
    pub should_start_research: bool,
    #[serde(default)]
    pub summary: String,
}

pub struct PlannerAgent;

impl Agent for PlannerAgent {
    fn name(&self) -> &'static str {
        "planner_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: PlannerRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid planner request payload: {e}"))?;

            let has_research_intent = detect_research_intent(&request.user_text).is_some();
            let intent = if has_research_intent {
                PlanIntent::Research
            } else {
                PlanIntent::Answer
            };

            let mut steps = vec!["route request".to_string()];
            if has_research_intent {
                steps.push("launch background research".to_string());
                steps.push("summarize research result".to_string());
                steps.push("create follow-up task".to_string());
                steps.push("prepare immediate acknowledgement".to_string());
            } else {
                steps.push("prepare direct answer".to_string());
            }
            steps.push("run chat response".to_string());

            let summary = match intent {
                PlanIntent::Research => "Need research workflow: start background research, summarize the findings, create a follow-up task, then respond with acknowledgement.".to_string(),
                PlanIntent::Answer => "Direct answer workflow: no background research needed; respond via chat agent.".to_string(),
            };

            let plan = WorkflowPlan {
                intent: intent.clone(),
                steps,
                should_start_research: matches!(intent, PlanIntent::Research),
                summary,
            };

            ctx.record_system_event(
                "adk_planner_plan_ready",
                Some(preview_text(&request.user_text)),
                Some("DONE"),
                Some(format!("intent={:?}", plan.intent)),
            );

            serde_json::to_value(plan).map_err(|e| e.to_string())
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

pub async fn run_planner_via_adk(
    request: PlannerRequest,
    tracker: Option<TaskTracker>,
) -> Result<WorkflowPlan, String> {
    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("planner_request")
        .with_optional_tracker(tracker)
        .with_metadata("agent", "planner_agent");

    runtime
        .run(&PlannerAgent, ctx, json!(request))
        .await
        .and_then(|output| serde_json::from_value(output).map_err(|e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn planner_marks_research_flows() {
        let plan = run_planner_via_adk(
            PlannerRequest {
                user_text: "幫我研究 Rust async runtime".to_string(),
                context_block: None,
                peer_id: None,
                fallback_reply: None,
                execution_result: None,
            },
            None,
        )
        .await
        .expect("planner should succeed");

        assert_eq!(plan.intent, PlanIntent::Research);
        assert!(plan.should_start_research);
        assert!(plan.steps.iter().any(|step| step.contains("summarize research result")));
        assert!(plan.steps.iter().any(|step| step.contains("create follow-up task")));
    }
}
