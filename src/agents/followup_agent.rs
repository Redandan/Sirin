use futures::FutureExt;
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::persona::TaskTracker;

pub struct FollowupWorkerAgent;

impl Agent for FollowupWorkerAgent {
    fn name(&self) -> &'static str {
        "followup_worker"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        _input: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let tracker = ctx
                .tracker()
                .cloned()
                .ok_or_else(|| "followup worker requires TaskTracker in AgentContext".to_string())?;

            ctx.record_system_event(
                "adk_followup_worker_started",
                None,
                Some("RUNNING"),
                Some("delegating to legacy followup loop".to_string()),
            );

            crate::followup::run_worker(tracker).await;
            Ok(json!({ "status": "stopped" }))
        }
        .boxed()
    }
}

pub async fn run_followup_worker_via_adk(tracker: TaskTracker) {
    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context_with_tracker("followup_worker", tracker)
        .with_metadata("agent", "followup_worker");
    let _ = runtime.run(&FollowupWorkerAgent, ctx, json!({})).await;
}
