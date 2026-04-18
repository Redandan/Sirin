use chrono::Utc;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::persona::TaskTracker;
use crate::researcher::{ResearchStatus, ResearchTask};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchRequest {
    pub topic: String,
    pub url: Option<String>,
}

pub struct ResearchAgent;

impl Agent for ResearchAgent {
    fn name(&self) -> &'static str {
        "research_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: ResearchRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid research request payload: {e}"))?;

            ctx.record_system_event(
                "adk_research_requested",
                Some(request.topic.clone()),
                Some("RUNNING"),
                request.url.clone(),
            );

            let memory_hits = ctx
                .call_tool("memory_search", json!({ "query": request.topic, "limit": 3 }))
                .await
                .unwrap_or_else(|_| json!([]));

            let web_hits = ctx
                .call_tool("web_search", json!({ "query": request.topic, "limit": 3 }))
                .await
                .unwrap_or_else(|_| json!([]));

            let recent_tasks = ctx
                .call_tool("task_recent", json!({ "limit": 5 }))
                .await
                .unwrap_or_else(|_| json!([]));

            let skill_catalog = ctx
                .call_tool("skill_catalog", json!({}))
                .await
                .unwrap_or_else(|_| json!([]));

            let memory_count = memory_hits.as_array().map(|items| items.len()).unwrap_or(0);
            let web_count = web_hits.as_array().map(|items| items.len()).unwrap_or(0);
            let task_count = recent_tasks.as_array().map(|items| items.len()).unwrap_or(0);
            let skill_count = skill_catalog.as_array().map(|items| items.len()).unwrap_or(0);
            ctx.record_system_event(
                "adk_research_preflight",
                Some(request.topic.clone()),
                Some("RUNNING"),
                Some(format!(
                    "memory_hits={memory_count}, web_hits={web_count}, recent_tasks={task_count}, skills={skill_count}"
                )),
            );

            let task = crate::researcher::run_research(request.topic, request.url).await;
            ctx.record_system_event(
                "adk_research_completed",
                Some(task.topic.clone()),
                Some(status_label(&task.status)),
                Some(format!("research_id={}", task.id)),
            );

            if task.status == ResearchStatus::Done {
                if let Some(summary) = research_summary(&task) {
                    let _ = ctx
                        .call_tool(
                            "task_record",
                            json!({
                                "event": "research_summary_ready",
                                "status": "DONE",
                                "message_preview": task.topic,
                                "reason": summary,
                                "correlation_id": task.id,
                            }),
                        )
                        .await;
                    ctx.record_system_event(
                        "adk_research_task_created",
                        Some(task.topic.clone()),
                        Some("DONE"),
                        Some("summary task recorded".to_string()),
                    );
                }
            }

            serde_json::to_value(&task).map_err(|e| e.to_string())
        }
        .boxed()
    }
}

fn status_label(status: &ResearchStatus) -> &'static str {
    match status {
        ResearchStatus::Done => "DONE",
        ResearchStatus::Running => "RUNNING",
        ResearchStatus::Failed => "FOLLOWUP_NEEDED",
    }
}

fn research_summary(task: &ResearchTask) -> Option<String> {
    task.final_report.as_ref().map(|report| {
        let snippet: String = report.chars().take(220).collect();
        format!("Research summary for '{}': {}", task.topic, snippet)
    })
}

fn fallback_failed_task(topic: String, url: Option<String>, error: String) -> ResearchTask {
    ResearchTask {
        id: format!("adk-failed-{}", Utc::now().timestamp_millis()),
        topic,
        url,
        status: ResearchStatus::Failed,
        steps: Vec::new(),
        final_report: Some(format!("ADK research agent failed: {error}")),
        started_at: Utc::now().to_rfc3339(),
        finished_at: Some(Utc::now().to_rfc3339()),
    }
}

pub async fn run_research_via_adk(topic: String, url: Option<String>) -> ResearchTask {
    run_research_via_adk_with_tracker(topic, url, None).await
}

pub async fn run_research_via_adk_with_tracker(
    topic: String,
    url: Option<String>,
    tracker: Option<TaskTracker>,
) -> ResearchTask {
    let request = ResearchRequest {
        topic: topic.clone(),
        url: url.clone(),
    };

    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("research_request")
        .with_optional_tracker(tracker)
        .with_metadata("agent", "research_agent")
        .with_metadata("topic", &topic);

    match runtime.run(&ResearchAgent, ctx, json!(request)).await {
        Ok(output) => serde_json::from_value(output).unwrap_or_else(|e| {
            fallback_failed_task(topic, url, format!("Invalid research agent output: {e}"))
        }),
        Err(err) => fallback_failed_task(topic, url, err),
    }
}
