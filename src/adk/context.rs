use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use chrono::Utc;
use serde_json::Value;

use crate::adk::tool::ToolRegistry;
use crate::llm::LlmConfig;
use crate::persona::{TaskEntry, TaskTracker};
use crate::sirin_log;

#[derive(Default)]
struct ExecutionTrace {
    tool_calls: Vec<String>,
    events: Vec<String>,
}

use crate::llm::{shared_http, shared_llm};

#[derive(Clone)]
pub struct AgentContext {
    pub request_id: String,
    pub source: String,
    pub tools: ToolRegistry,
    pub metadata: HashMap<String, String>,
    /// Process-wide shared HTTP client (cheap to clone — internally Arc).
    pub http: Arc<reqwest::Client>,
    /// Process-wide LLM configuration read once from environment.
    pub llm: Arc<LlmConfig>,
    tracker: Option<TaskTracker>,
    trace: Arc<Mutex<ExecutionTrace>>,
    /// Optional recent-conversation snippet injected by the caller so agents
    /// have awareness of what the user was just discussing.
    context_hint: Option<String>,
}

impl AgentContext {
    pub fn new(source: impl Into<String>, tools: ToolRegistry) -> Self {
        Self {
            request_id: format!("adk-{}", Utc::now().timestamp_millis()),
            source: source.into(),
            tools,
            metadata: HashMap::new(),
            http: shared_http(),
            llm: shared_llm(),
            tracker: None,
            trace: Arc::new(Mutex::new(ExecutionTrace::default())),
            context_hint: None,
        }
    }

    pub fn with_tracker(mut self, tracker: TaskTracker) -> Self {
        self.tracker = Some(tracker);
        self
    }

    pub fn with_optional_tracker(mut self, tracker: Option<TaskTracker>) -> Self {
        self.tracker = tracker;
        self
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    pub fn with_context_hint(mut self, hint: Option<String>) -> Self {
        self.context_hint = hint;
        self
    }

    pub fn context_hint(&self) -> Option<&str> {
        self.context_hint.as_deref()
    }

    pub fn tracker(&self) -> Option<&TaskTracker> {
        self.tracker.as_ref()
    }

    pub fn tool_calls_snapshot(&self) -> Vec<String> {
        let trace = self.trace.lock().expect("AgentContext trace mutex poisoned");
        let mut seen = HashSet::new();
        trace
            .tool_calls
            .iter()
            .filter(|name| seen.insert((*name).clone()))
            .cloned()
            .collect()
    }

    pub fn event_trace_snapshot(&self) -> Vec<String> {
        self.trace
            .lock()
            .expect("AgentContext trace mutex poisoned")
            .events
            .clone()
    }

    fn push_tool_call(&self, name: &str) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.tool_calls.push(name.to_string());
            if trace.tool_calls.len() > 32 {
                let excess = trace.tool_calls.len() - 32;
                trace.tool_calls.drain(0..excess);
            }
        }
    }

    fn push_event_trace(&self, note: impl Into<String>) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.events.push(note.into());
            if trace.events.len() > 32 {
                let excess = trace.events.len() - 32;
                trace.events.drain(0..excess);
            }
        }
    }

    pub async fn call_tool(&self, name: &str, input: Value) -> Result<Value, String> {
        self.push_tool_call(name);
        let result = self.tools.call(self, name, input).await;
        let outcome = if result.is_ok() { "ok" } else { "error" };
        self.push_event_trace(format!("tool:{name}:{outcome}"));
        result
    }

    pub fn record_system_event(
        &self,
        event: impl Into<String>,
        message_preview: Option<String>,
        status: Option<&str>,
        reason: Option<String>,
    ) {
        let event = event.into();
        if let Some(reason_text) = reason.as_deref() {
            sirin_log!("[adk:{}] {} — {}", self.source, event, reason_text);
        } else {
            sirin_log!("[adk:{}] {}", self.source, event);
        }

        let trace_note = match (status, reason.as_deref()) {
            (Some(status), Some(reason)) => format!("event:{event}:{status}:{reason}"),
            (Some(status), None) => format!("event:{event}:{status}"),
            (None, Some(reason)) => format!("event:{event}:{reason}"),
            (None, None) => format!("event:{event}"),
        };
        self.push_event_trace(trace_note);

        if let Some(tracker) = &self.tracker {
            let entry = TaskEntry::system_event(
                "Sirin",
                event,
                message_preview,
                status,
                reason,
                Some(self.request_id.clone()),
            );
            let _ = tracker.record(&entry);
        }
    }
}
