use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::llm::call_prompt;
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

            // ── Try LLM-based planning first ──────────────────────────────────
            let plan = match llm_plan(ctx, &request).await {
                Some(p) => {
                    ctx.record_system_event(
                        "adk_planner_plan_ready",
                        Some(preview_text(&request.user_text)),
                        Some("DONE"),
                        Some(format!("intent={:?} source=llm", p.intent)),
                    );
                    p
                }
                // ── Fallback: keyword heuristic ───────────────────────────────
                None => {
                    let has_research = detect_research_intent(&request.user_text).is_some();
                    let intent = if has_research { PlanIntent::Research } else { PlanIntent::Answer };
                    let mut steps = vec!["route request".to_string()];
                    if has_research {
                        steps.extend_from_slice(&[
                            "launch background research".to_string(),
                            "summarize research result".to_string(),
                            "create follow-up task".to_string(),
                            "prepare immediate acknowledgement".to_string(),
                        ]);
                    } else {
                        steps.push("prepare direct answer".to_string());
                    }
                    steps.push("run chat response".to_string());
                    let summary = match intent {
                        PlanIntent::Research => "Need research workflow (heuristic fallback).".to_string(),
                        PlanIntent::Answer => "Direct answer workflow (heuristic fallback).".to_string(),
                    };
                    let p = WorkflowPlan {
                        should_start_research: matches!(intent, PlanIntent::Research),
                        intent,
                        steps,
                        summary,
                    };
                    ctx.record_system_event(
                        "adk_planner_plan_ready",
                        Some(preview_text(&request.user_text)),
                        Some("DONE"),
                        Some(format!("intent={:?} source=heuristic", p.intent)),
                    );
                    p
                }
            };

            serde_json::to_value(plan).map_err(|e| e.to_string())
        }
        .boxed()
    }
}

/// Ask the LLM to classify the user message and return a structured plan.
/// Returns `None` on LLM error or unparseable output so the caller can fall back.
async fn llm_plan(ctx: &AgentContext, request: &PlannerRequest) -> Option<WorkflowPlan> {
    let context_hint = request
        .context_block
        .as_deref()
        .map(|c| format!("\nRecent context:\n{c}"))
        .unwrap_or_default();

    let prompt = format!(
        r#"You are a planning assistant. Classify the user's message and output ONLY valid JSON.

User message: "{msg}"{context_hint}

JSON schema (fill every field):
{{
  "intent": "answer" | "research",
  "should_start_research": true | false,
  "steps": ["step 1", "step 2", ...],
  "summary": "one-sentence description of the workflow"
}}

Rules:
- Use "research" when the user wants an investigation, analysis, or information about a URL/topic.
- Use "answer" for greetings, simple questions, or direct instructions.
- steps must be an ordered list of 2-5 concrete actions.

Output only the JSON object, no explanation."#,
        msg = request.user_text,
        context_hint = context_hint,
    );

    let raw = call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt)
        .await
        .ok()?;

    // Extract JSON object from the response (model may wrap it in prose).
    let json_start = raw.find('{')?;
    let json_end = raw.rfind('}').map(|i| i + 1)?;
    let json_slice = &raw[json_start..json_end];

    #[derive(serde::Deserialize)]
    struct LlmPlanRaw {
        intent: String,
        #[serde(default)]
        should_start_research: bool,
        #[serde(default)]
        steps: Vec<String>,
        #[serde(default)]
        summary: String,
    }

    let parsed: LlmPlanRaw = serde_json::from_str(json_slice).ok()?;

    let intent = match parsed.intent.to_lowercase().as_str() {
        "research" => PlanIntent::Research,
        _ => PlanIntent::Answer,
    };

    Some(WorkflowPlan {
        should_start_research: parsed.should_start_research
            || matches!(intent, PlanIntent::Research),
        intent,
        steps: if parsed.steps.is_empty() {
            vec!["route request".to_string(), "run chat response".to_string()]
        } else {
            parsed.steps
        },
        summary: if parsed.summary.is_empty() {
            "LLM-generated plan".to_string()
        } else {
            parsed.summary
        },
    })
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
