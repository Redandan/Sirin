use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use chrono::Utc;
use futures::{future::BoxFuture, FutureExt};
use serde_json::{json, Value};

use crate::adk::context::AgentContext;
use crate::persona::{BehaviorEngine, IncomingMessage, Persona, TaskEntry};

pub type ToolResult = Result<Value, String>;
pub type ToolFuture<'a> = BoxFuture<'a, ToolResult>;
pub type ToolHandler = Arc<dyn for<'a> Fn(&'a AgentContext, Value) -> ToolFuture<'a> + Send + Sync>;

#[derive(Clone, Default)]
pub struct ToolRegistry {
    handlers: Arc<HashMap<String, ToolHandler>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_fn<F, Fut>(self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.register_ctx_fn(name, move |_ctx, input| {
            let handler = Arc::clone(&handler);
            async move { (handler)(input).await }.boxed()
        })
    }

    pub fn register_ctx_fn<F>(self, name: impl Into<String>, handler: F) -> Self
    where
        F: for<'a> Fn(&'a AgentContext, Value) -> ToolFuture<'a> + Send + Sync + 'static,
    {
        let mut handlers = (*self.handlers).clone();
        handlers.insert(name.into(), Arc::new(handler));
        Self {
            handlers: Arc::new(handlers),
        }
    }

    pub async fn call(&self, ctx: &AgentContext, name: &str, input: Value) -> ToolResult {
        let handler = self
            .handlers
            .get(name)
            .cloned()
            .ok_or_else(|| format!("Tool not registered: {name}"))?;
        handler(ctx, input).await
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort();
        names
    }
}

fn query_from_input(input: &Value) -> Result<String, String> {
    input
        .get("query")
        .and_then(Value::as_str)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "Missing 'query' string".to_string())
}

fn limit_from_input(input: &Value, default_limit: usize) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .filter(|&v| v > 0)
        .unwrap_or(default_limit)
}

fn optional_string_field(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required_string_field(input: &Value, key: &str) -> Result<String, String> {
    optional_string_field(input, key).ok_or_else(|| format!("Missing '{key}' string"))
}

pub fn default_tool_registry() -> ToolRegistry {
    ToolRegistry::new()
        .register_fn("web_search", |input| async move {
            let query = query_from_input(&input)?;
            let limit = limit_from_input(&input, 5);
            let results = crate::skills::ddg_search(&query).await?;
            Ok(json!(results.into_iter().take(limit).collect::<Vec<_>>()))
        })
        .register_fn("memory_search", |input| async move {
            let query = query_from_input(&input)?;
            let limit = limit_from_input(&input, 5);
            let results = crate::memory::memory_search(&query, limit)
                .map_err(|e| e.to_string())?;
            Ok(json!(results))
        })
        .register_fn("codebase_search", |input| async move {
            let query = query_from_input(&input)?;
            let limit = limit_from_input(&input, 5);
            let results = crate::memory::search_codebase(&query, limit)
                .map_err(|e| e.to_string())?;
            Ok(json!(results))
        })
        .register_fn("project_overview", |input| async move {
            let limit = limit_from_input(&input, 8);
            let files = crate::memory::list_project_files(limit)
                .map_err(|e| e.to_string())?;

            Ok(json!({
                "summary": "Sirin 是一個用 Rust 建構的本地 AI 助手專案，包含 egui 桌面 UI、Telegram 整合、ADK 風格 agent 流程、記憶 / 程式碼索引，以及本地 LLM 支援。",
                "files": files,
            }))
        })
        .register_fn("local_file_read", |input| async move {
            let path = optional_string_field(&input, "path")
                .or_else(|| optional_string_field(&input, "query"))
                .ok_or_else(|| "Missing 'path' string".to_string())?;
            let max_chars = input
                .get("max_chars")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(2400);
            let content = crate::memory::inspect_project_file(&path, max_chars)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "path": path,
                "content": content,
            }))
        })
        .register_ctx_fn("task_recent", |ctx, input| {
            async move {
                let limit = limit_from_input(&input, 20);
                let tracker = ctx
                    .tracker()
                    .cloned()
                    .ok_or_else(|| "task_recent requires TaskTracker in AgentContext".to_string())?;
                let entries = tracker.read_last_n(limit).map_err(|e| e.to_string())?;
                serde_json::to_value(entries).map_err(|e| e.to_string())
            }
            .boxed()
        })
        .register_ctx_fn("task_lookup", |ctx, input| {
            async move {
                let timestamp = required_string_field(&input, "timestamp")?;
                let tracker = ctx
                    .tracker()
                    .cloned()
                    .ok_or_else(|| "task_lookup requires TaskTracker in AgentContext".to_string())?;
                let entry = tracker
                    .find_by_timestamp(&timestamp)
                    .map_err(|e| e.to_string())?;
                serde_json::to_value(entry).map_err(|e| e.to_string())
            }
            .boxed()
        })
        .register_ctx_fn("task_record", |ctx, input| {
            async move {
                let event = required_string_field(&input, "event")?;
                let tracker = ctx
                    .tracker()
                    .cloned()
                    .ok_or_else(|| "task_record requires TaskTracker in AgentContext".to_string())?;
                let status = optional_string_field(&input, "status");
                let message_preview = optional_string_field(&input, "message_preview");
                let reason = optional_string_field(&input, "reason");
                let correlation_id = optional_string_field(&input, "correlation_id")
                    .or_else(|| Some(ctx.request_id.clone()));
                let entry = TaskEntry::system_event(
                    "Sirin",
                    event,
                    message_preview,
                    status.as_deref(),
                    reason,
                    correlation_id,
                );
                tracker.record(&entry).map_err(|e| e.to_string())?;
                serde_json::to_value(entry).map_err(|e| e.to_string())
            }
            .boxed()
        })
        .register_fn("research_lookup", |input| async move {
            let id = required_string_field(&input, "id")?;
            let task = crate::researcher::get_research(&id)?;
            serde_json::to_value(task).map_err(|e| e.to_string())
        })
        .register_fn("skill_catalog", |input| async move {
            if let Some(query) = optional_string_field(&input, "query") {
                let recommended = crate::skills::recommended_skills(&query);
                if !recommended.is_empty() {
                    return Ok(json!(recommended));
                }
            }
            Ok(json!(crate::skills::list_skills()))
        })
        .register_fn("skill_execute", |input| async move {
            let skill_id = required_string_field(&input, "skill_id")?;
            let timestamp = optional_string_field(&input, "timestamp")
                .unwrap_or_else(|| Utc::now().to_rfc3339());
            let result = crate::skills::execute_skill(&skill_id, &timestamp)?;
            Ok(json!(result))
        })
        .register_ctx_fn("behavior_evaluate", |ctx, input| {
            async move {
                let persona = Persona::load().map_err(|e| e.to_string())?;
                let msg = required_string_field(&input, "msg")?;
                let source = optional_string_field(&input, "source")
                    .unwrap_or_else(|| ctx.source.clone());
                let estimated_value = input
                    .get("estimated_value")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let should_record = input
                    .get("record")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                let incoming = IncomingMessage { source, msg };
                let decision = BehaviorEngine::evaluate(incoming, estimated_value, &persona);

                if should_record {
                    if let Some(tracker) = ctx.tracker() {
                        let entry = TaskEntry::behavior_decision(&persona, estimated_value, &decision);
                        tracker.record(&entry).map_err(|e| e.to_string())?;
                    }
                }

                Ok(json!({
                    "draft": decision.draft,
                    "high_priority": decision.high_priority,
                    "matched_objective": decision.matched_objective,
                    "tier": decision.tier,
                    "reason": decision.reason,
                }))
            }
            .boxed()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn custom_registry_round_trips_values() {
        let registry = ToolRegistry::new().register_fn("echo", |input| async move { Ok(input) });
        let ctx = AgentContext::new("test", registry.clone());
        let output = ctx
            .call_tool("echo", json!({ "hello": "world" }))
            .await
            .expect("echo tool should succeed");

        assert_eq!(output["hello"], "world");
    }

    #[tokio::test]
    async fn default_registry_exposes_skill_catalog() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let output = ctx
            .call_tool("skill_catalog", json!({}))
            .await
            .expect("skill catalog should be available");

        assert!(output.as_array().map(|items| !items.is_empty()).unwrap_or(false));
        assert!(ctx.tools.names().iter().any(|name| name == "project_overview"));
    }
}
