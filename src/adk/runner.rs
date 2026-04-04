use crate::adk::{agent::Agent, context::AgentContext, tool::{default_tool_registry, ToolRegistry}};
use crate::persona::TaskTracker;

#[derive(Clone)]
pub struct AgentRuntime {
    tools: ToolRegistry,
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::new(default_tool_registry())
    }
}

impl AgentRuntime {
    pub fn new(tools: ToolRegistry) -> Self {
        Self { tools }
    }

    pub fn context(&self, source: impl Into<String>) -> AgentContext {
        AgentContext::new(source, self.tools.clone())
    }

    pub fn context_with_tracker(
        &self,
        source: impl Into<String>,
        tracker: TaskTracker,
    ) -> AgentContext {
        self.context(source).with_tracker(tracker)
    }

    pub async fn run<A: Agent>(
        &self,
        agent: &A,
        ctx: AgentContext,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let tool_list = ctx.tools.names().join(", ");
        ctx.record_system_event(
            format!("adk:{}:start", agent.name()),
            None,
            Some("RUNNING"),
            Some(format!("tools=[{tool_list}]")),
        );

        let result = agent.run(&ctx, input).await;

        match &result {
            Ok(_) => ctx.record_system_event(
                format!("adk:{}:done", agent.name()),
                None,
                Some("DONE"),
                None,
            ),
            Err(err) => ctx.record_system_event(
                format!("adk:{}:error", agent.name()),
                None,
                Some("FOLLOWUP_NEEDED"),
                Some(err.clone()),
            ),
        }

        result
    }
}
