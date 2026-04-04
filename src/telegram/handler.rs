use std::future::Future;

use crate::persona::TaskTracker;

use super::{
    commands::execute_user_request,
    config::TelegramConfig,
};

pub struct ReplyPlan {
    pub execution_result: Option<String>,
    pub fallback_reply: String,
    pub should_record_ai_decision: bool,
}

pub async fn prepare_reply_plan<F, Fut>(
    text: &str,
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
            peer_id: None,
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

    let research_execution = if route == "research" {
        let research_request = routed
            .as_ref()
            .and_then(|value| value.get("research_request"))
            .cloned();

        if let Some(payload) = research_request {
            if let Ok(request) = serde_json::from_value::<crate::agents::research_agent::ResearchRequest>(payload) {
                let topic_for_msg = request.topic.clone();
                let url_for_msg = request.url.clone();
                start_research(request.topic, request.url).await;
                let url_hint = url_for_msg
                    .map(|value| format!(" ({value})"))
                    .unwrap_or_default();
                Some(format!(
                    "執行結果：已啟動背景調研任務「{}{}」，完成後結果將記錄在任務板。",
                    topic_for_msg, url_hint
                ))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let execution_result = research_execution.or_else(|| execute_user_request(text, tracker, persona_name));
    let fallback_reply = cfg
        .auto_reply_text
        .replace("{persona}", persona_name)
        .replace("{voice}", voice)
        .replace("{ack_prefix}", ack_prefix)
        .replace("{compliance}", compliance);

    ReplyPlan {
        should_record_ai_decision: execution_result.is_some(),
        execution_result,
        fallback_reply,
    }
}
