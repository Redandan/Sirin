use std::future::Future;

use crate::persona::TaskTracker;

use super::{
    commands::execute_user_request,
    config::TelegramConfig,
};

pub struct ReplyPlan {
    /// The `chat_request` JSON object forwarded from the Router, complete with
    /// planner intent hints and (for research routes) a language-neutral
    /// execution_result.  `None` when the router call fails.
    pub router_chat_request: Option<serde_json::Value>,
    /// Result from a side-command (e.g. "todo …", "查詢待辦") that was executed
    /// directly — not through the Router.  When present, overrides the
    /// execution_result that the router may have embedded in chat_request.
    pub command_execution_result: Option<String>,
    pub fallback_reply: String,
    pub should_record_ai_decision: bool,
}

pub async fn prepare_reply_plan<F, Fut>(
    text: &str,
    peer_id: Option<i64>,
    persona_name: &str,
    voice: &str,
    ack_prefix: &str,
    compliance: &str,
    tracker: &TaskTracker,
    cfg: &TelegramConfig,
    start_research: F,
) -> ReplyPlan
where
    F: FnOnce(String, Option<String>) -> Fut,
    Fut: Future<Output = ()>,
{
    let routed = crate::agents::router_agent::run_router_via_adk(
        crate::agents::router_agent::RouterRequest {
            user_text: text.to_string(),
            context_block: None,
            peer_id,
            fallback_reply: None,
            execution_result: None,
        },
        Some(tracker.clone()),
    )
    .await
    .ok();

    let route = routed
        .as_ref()
        .and_then(|value| value.get("route"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("chat");

    // Extract the router-built chat_request (carries planner hints + execution_result).
    let router_chat_request = routed
        .as_ref()
        .and_then(|value| value.get("chat_request").cloned());

    // If the router chose the research route, launch the background task.
    // The router already embedded the language-neutral execution_result into
    // chat_request — no need to build a separate string here.
    if route == "research" {
        let research_request = routed
            .as_ref()
            .and_then(|value| value.get("research_request"))
            .cloned();

        if let Some(payload) = research_request {
            if let Ok(request) =
                serde_json::from_value::<crate::agents::research_agent::ResearchRequest>(payload)
            {
                start_research(request.topic, request.url).await;
            }
        }
    }

    // Side commands (todo creation, task queries) are executed independently
    // of routing and their result overrides whatever execution_result the router set.
    let command_execution_result = execute_user_request(text, tracker, persona_name);

    let fallback_reply = cfg
        .auto_reply_text
        .replace("{persona}", persona_name)
        .replace("{voice}", voice)
        .replace("{ack_prefix}", ack_prefix)
        .replace("{compliance}", compliance);

    let should_record_ai_decision = command_execution_result.is_some() || route == "research";

    ReplyPlan {
        router_chat_request,
        command_execution_result,
        fallback_reply,
        should_record_ai_decision,
    }
}
