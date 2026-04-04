use futures::future::BoxFuture;
use serde_json::Value;

use crate::adk::context::AgentContext;

pub type AgentInput = Value;
pub type AgentOutput = Value;
pub type AgentResult = Result<AgentOutput, String>;

pub trait Agent: Send + Sync {
    fn name(&self) -> &'static str;

    fn run<'a>(&'a self, ctx: &'a AgentContext, input: AgentInput) -> BoxFuture<'a, AgentResult>;
}
