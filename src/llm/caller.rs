//! Pluggable LLM call surface for agents — Issue #263 Phase 2.
//!
//! Background: `src/agents/coding_agent/react.rs` and friends reach
//! [`crate::llm::call_coding_prompt`] (and siblings) directly, threading
//! `ctx.http` and `ctx.llm` through manually.  That hard-wires every
//! agent test to either spin up a real HTTP client + provider OR skip the
//! LLM-driven path entirely (which is why coverage for `react.rs` /
//! `verify.rs` / `dispatch.rs` was 0% before this batch).
//!
//! [`LlmCaller`] is a thin trait over the four `call_*_prompt` entry
//! points.  Real code uses [`RealLlmCaller`] (just delegates).  Tests use
//! [`MockLlmCaller`] to inject deterministic responses and assert what
//! prompts the agent generated.
//!
//! ## Why hand-rolled `Pin<Box<...>>` instead of `async_trait`
//!
//! Sirin doesn't pull in `async_trait`.  The MCP registry (#257) already
//! uses the same `Pin<Box<dyn Future<...> + Send + 'a>>` shape on its
//! `AsyncJson` variant, so this stays stylistically consistent and
//! avoids a new compile-time dep for one trait.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::{call_coding_prompt, call_large_prompt, call_prompt, call_router_prompt, LlmConfig};

/// Lifetime-bounded boxed future returned by the trait methods.  The
/// lifetime is tied to `&self` so impls can borrow internal state
/// (HTTP client, mock state) for the duration of the call without
/// requiring `'static`.
pub type BoxLlmFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// Abstraction over the LLM call surface used by agents.
///
/// One method per `call_*_prompt` function in [`crate::llm`].  A future
/// addition (e.g. `call_vision`) can be added here when an agent needs
/// it; today only the four text-prompt entry points are wired.
pub trait LlmCaller: Send + Sync {
    /// Coding-tier model — used by `coding_agent::react` / `verify` /
    /// `prompt`.  On 429 from primary, falls back to the fallback LLM.
    fn call_coding<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a>;

    /// Router-tier model — used by `chat_agent::dispatch` /
    /// `planner_agent` / `chat_agent::intent`.  Smaller, faster model
    /// kept resident in VRAM via `keep_alive=-1` on Ollama.
    fn call_router<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a>;

    /// Large/powerful model — currently `#[allow(dead_code)]` in the
    /// real code but exposed here for future agent use.
    fn call_large<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a>;

    /// Plain main-tier model — used by `chat_agent::context`.
    fn call_plain<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a>;
}

// ── Real implementation ─────────────────────────────────────────────────────

/// Production [`LlmCaller`] — bundles an HTTP client + an LLM config and
/// delegates each call to the corresponding `crate::llm::call_*_prompt`
/// function.
pub struct RealLlmCaller {
    pub http: Arc<reqwest::Client>,
    pub llm:  Arc<LlmConfig>,
}

impl RealLlmCaller {
    pub fn new(http: Arc<reqwest::Client>, llm: Arc<LlmConfig>) -> Self {
        Self { http, llm }
    }

    /// Convenience constructor that pulls both halves from the
    /// process-wide singletons.
    pub fn from_globals() -> Self {
        Self::new(super::shared_http(), super::shared_llm())
    }
}

impl LlmCaller for RealLlmCaller {
    fn call_coding<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a> {
        Box::pin(async move {
            call_coding_prompt(&self.http, &self.llm, prompt)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn call_router<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a> {
        Box::pin(async move {
            call_router_prompt(&self.http, &self.llm, prompt)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn call_large<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a> {
        Box::pin(async move {
            call_large_prompt(&self.http, &self.llm, prompt)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn call_plain<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a> {
        Box::pin(async move {
            call_prompt(&self.http, &self.llm, prompt)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

// ── Mock implementation (test helper) ───────────────────────────────────────

/// Identifies which `call_*` method the mock served, for assertion in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmKind {
    Coding,
    Router,
    Large,
    Plain,
}

/// Test [`LlmCaller`] — serves a fixed sequence of responses and records
/// every prompt it received.
///
/// ## Usage
///
/// ```ignore
/// let mock = Arc::new(MockLlmCaller::with_responses(vec![
///     Ok("THOUGHT: …\nACTION: DONE\nFINAL_ANSWER: ok".into()),
/// ]));
/// let ctx = AgentContext::new("test", tools).with_llm_caller(mock.clone());
/// // … run the agent …
/// // Inspect what the agent sent:
/// let captured = mock.captured.lock().unwrap();
/// assert_eq!(captured.len(), 1);
/// assert_eq!(captured[0].0, LlmKind::Coding);
/// ```
///
/// Panics if the agent calls more times than responses provided — that's
/// usually a sign the test setup is incomplete (missing trailing `DONE`).
///
/// Always compiled (not gated on `#[cfg(test)]`) so integration tests in
/// other modules can use it without each one toggling a feature flag.
pub struct MockLlmCaller {
    responses: std::sync::Mutex<std::collections::VecDeque<Result<String, String>>>,
    /// Captured (kind, prompt) pairs in call order.  Public so tests can
    /// inspect.  Wrapped in `Mutex` because trait methods take `&self`.
    pub captured: std::sync::Mutex<Vec<(LlmKind, String)>>,
}

impl MockLlmCaller {
    /// Construct with a queue of responses to serve in order.
    pub fn with_responses(responses: Vec<Result<String, String>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
            captured:  std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Number of times any `call_*` method has been invoked.
    pub fn call_count(&self) -> usize {
        self.captured.lock().expect("captured poisoned").len()
    }

    /// Snapshot of every prompt the agent has sent, in order.
    pub fn prompts(&self) -> Vec<String> {
        self.captured
            .lock()
            .expect("captured poisoned")
            .iter()
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn next_response(&self, kind: LlmKind, prompt: String) -> Result<String, String> {
        self.captured
            .lock()
            .expect("captured poisoned")
            .push((kind, prompt));
        self.responses
            .lock()
            .expect("responses poisoned")
            .pop_front()
            .unwrap_or_else(|| {
                panic!("MockLlmCaller exhausted (kind={kind:?}) — add more responses")
            })
    }
}

impl LlmCaller for MockLlmCaller {
    fn call_coding<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a> {
        let resp = self.next_response(LlmKind::Coding, prompt);
        Box::pin(async move { resp })
    }

    fn call_router<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a> {
        let resp = self.next_response(LlmKind::Router, prompt);
        Box::pin(async move { resp })
    }

    fn call_large<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a> {
        let resp = self.next_response(LlmKind::Large, prompt);
        Box::pin(async move { resp })
    }

    fn call_plain<'a>(&'a self, prompt: String) -> BoxLlmFuture<'a> {
        let resp = self.next_response(LlmKind::Plain, prompt);
        Box::pin(async move { resp })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_serves_responses_in_order() {
        let mock = MockLlmCaller::with_responses(vec![
            Ok("first".into()),
            Ok("second".into()),
        ]);
        assert_eq!(mock.call_coding("p1".into()).await.unwrap(), "first");
        assert_eq!(mock.call_router("p2".into()).await.unwrap(), "second");
        assert_eq!(mock.call_count(), 2);
    }

    #[tokio::test]
    async fn mock_records_prompts_and_kinds() {
        let mock = MockLlmCaller::with_responses(vec![
            Ok("a".into()),
            Ok("b".into()),
        ]);
        mock.call_coding("prompt-c".into()).await.unwrap();
        mock.call_router("prompt-r".into()).await.unwrap();
        let captured = mock.captured.lock().unwrap();
        assert_eq!(captured[0], (LlmKind::Coding, "prompt-c".to_string()));
        assert_eq!(captured[1], (LlmKind::Router, "prompt-r".to_string()));
    }

    #[tokio::test]
    async fn mock_propagates_errors() {
        let mock = MockLlmCaller::with_responses(vec![Err("boom".into())]);
        let result = mock.call_coding("p".into()).await;
        assert_eq!(result, Err("boom".into()));
    }

    #[tokio::test]
    #[should_panic(expected = "MockLlmCaller exhausted")]
    async fn mock_panics_when_responses_exhausted() {
        let mock = MockLlmCaller::with_responses(vec![]);
        let _ = mock.call_coding("p".into()).await;
    }

    #[test]
    fn mock_prompts_returns_in_order() {
        let mock = MockLlmCaller::with_responses(vec![
            Ok("r1".into()),
            Ok("r2".into()),
        ]);
        // futures::executor would work, but tokio::runtime::Handle::current()
        // isn't available in a sync test — use a small block_on instead.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            mock.call_coding("first".into()).await.unwrap();
            mock.call_router("second".into()).await.unwrap();
        });
        assert_eq!(mock.prompts(), vec!["first".to_string(), "second".to_string()]);
    }
}
