//! Per-call token usage telemetry (Issue #240).
//!
//! Backends emit [`TokenUsage`] records into a [`tokio::task_local`] collector
//! after every successful HTTP round-trip.  Callers (test_runner) wrap an async
//! body in [`with_recording`] to receive the aggregated tally for that body.
//!
//! Outside a `with_recording` scope, `record()` is a silent no-op — UI calls,
//! agent chats, MCP tool dispatches all coexist on the same backends without
//! polluting test_runner stats.
//!
//! ## Why task-local, not thread-local
//!
//! Tokio multiplexes async tasks across worker threads.  A single
//! `run_test_async` future can be polled on three different threads during one
//! test.  Thread-local storage would lose half the records.  Task-local is the
//! one primitive that follows a future across `.await` points.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

// ── Types ────────────────────────────────────────────────────────────────────

/// Token usage from a single LLM call.  Populated from the OpenAI-compat
/// `usage` field; missing fields default to 0.
///
/// Field naming follows OpenAI's response shape so we can deserialise
/// straight from JSON without remapping.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens charged at the input rate.
    #[serde(default)]
    pub prompt_tokens:     u32,
    /// Output / completion tokens charged at the output rate.
    #[serde(default)]
    pub completion_tokens: u32,
    /// Cached input tokens — charged at a discounted rate (typically 10%
    /// of input).  Anthropic returns this as `cache_read_input_tokens`;
    /// OpenAI returns it as `prompt_tokens_details.cached_tokens`.
    /// Backends normalise into this single field.
    #[serde(default)]
    pub cached_tokens:     u32,
    /// Resolved model that produced this call (e.g. `claude-sonnet-4-6`,
    /// `gemini-2.0-flash`).  Used by [`cost_usd`] to look up the rate.
    #[serde(default)]
    pub model:             String,
}

impl TokenUsage {
    /// Sum two records into a single aggregate, choosing the most-specific
    /// model name (longer wins).
    pub fn add(mut self, other: &Self) -> Self {
        self.prompt_tokens     = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self.completion_tokens.saturating_add(other.completion_tokens);
        self.cached_tokens     = self.cached_tokens.saturating_add(other.cached_tokens);
        if other.model.len() > self.model.len() {
            self.model = other.model.clone();
        }
        self
    }

    /// Estimated cost in USD using a per-model price table.  Conservative —
    /// rounds *up* unknown models to a sensible default rate so we never
    /// understate cost.
    pub fn cost_usd(&self) -> f64 {
        let rate = price_per_million(&self.model);
        let billed_input = self.prompt_tokens.saturating_sub(self.cached_tokens) as f64;
        let cached       = self.cached_tokens as f64;
        let output       = self.completion_tokens as f64;
        (billed_input * rate.input
            + cached     * rate.cached_input
            + output     * rate.output) / 1_000_000.0
    }
}

// ── Price table (USD / 1M tokens) ────────────────────────────────────────────

#[derive(Copy, Clone)]
pub struct ModelRate {
    pub input:        f64,
    /// Discounted rate for cache-hit input tokens.  Typically 10% of input
    /// for Anthropic; 50% for OpenAI prompt-cache; 25% for Gemini implicit
    /// cache.
    pub cached_input: f64,
    pub output:       f64,
}

/// Look up the USD/1M rate for a model name.  Substring matching — first
/// matching prefix wins, falling back to a conservative default.
///
/// Prices accurate as of 2026-05; review quarterly.  Off by a small factor
/// is fine; this is for "is test X 10× more expensive than test Y" not
/// finance.
pub fn price_per_million(model: &str) -> ModelRate {
    let m = model.to_ascii_lowercase();

    // Anthropic Claude family
    if m.contains("opus") {
        return ModelRate { input: 15.0, cached_input: 1.50, output: 75.0 };
    }
    if m.contains("sonnet") {
        return ModelRate { input: 3.0,  cached_input: 0.30, output: 15.0 };
    }
    if m.contains("haiku") {
        return ModelRate { input: 0.80, cached_input: 0.08, output: 4.0  };
    }

    // Google Gemini family
    if m.contains("gemini-2.5-pro") {
        return ModelRate { input: 1.25, cached_input: 0.31, output: 10.0 };
    }
    if m.contains("gemini-2.5-flash") {
        return ModelRate { input: 0.30, cached_input: 0.075, output: 2.50 };
    }
    if m.contains("gemini-2.0-flash") || m.contains("gemini-flash") {
        return ModelRate { input: 0.10, cached_input: 0.025, output: 0.40 };
    }

    // OpenAI family
    if m.contains("gpt-4o-mini") || m.contains("4o-mini") {
        return ModelRate { input: 0.15, cached_input: 0.075, output: 0.60 };
    }
    if m.contains("gpt-4o") || m.contains("4o") {
        return ModelRate { input: 2.50, cached_input: 1.25, output: 10.0 };
    }
    if m.contains("gpt-4") {
        return ModelRate { input: 10.0, cached_input: 5.0, output: 30.0 };
    }

    // DeepSeek (used as fallback)
    if m.contains("deepseek-chat") || m.contains("deepseek-v") {
        return ModelRate { input: 0.27, cached_input: 0.07, output: 1.10 };
    }

    // Local backends (Ollama / LM Studio) — free
    if m.contains("llama") || m.contains("qwen") || m.contains("mistral")
        || m.contains("gemma") || m.contains("phi")
    {
        return ModelRate { input: 0.0, cached_input: 0.0, output: 0.0 };
    }

    // Unknown model — conservative default (Claude Sonnet ballpark) so we
    // don't understate cost in dashboards.
    ModelRate { input: 3.0, cached_input: 0.30, output: 15.0 }
}

// ── Task-local collector ─────────────────────────────────────────────────────

tokio::task_local! {
    static USAGE_COLLECTOR: Arc<Mutex<Vec<TokenUsage>>>;
}

/// Run `fut` with a fresh usage collector in scope.  Returns the future's
/// output paired with every [`TokenUsage`] emitted from inside the scope.
///
/// Nested scopes work — the inner scope captures only its own calls; the
/// outer scope sees the inner ones too if the inner's future returns
/// before the outer drains.  In practice we only nest by accident; the
/// test runner is the sole intended caller.
pub async fn with_recording<F, T>(fut: F) -> (T, Vec<TokenUsage>)
where
    F: std::future::Future<Output = T>,
{
    let collector: Arc<Mutex<Vec<TokenUsage>>> = Arc::new(Mutex::new(Vec::new()));
    let result = USAGE_COLLECTOR.scope(collector.clone(), fut).await;
    let drained = std::mem::take(
        &mut *collector.lock().unwrap_or_else(|e| e.into_inner())
    );
    (result, drained)
}

/// Record a usage event from inside a `with_recording` scope.  No-op when
/// called outside a scope (UI calls, agent chats, MCP tools won't pollute
/// test_runner stats).
pub fn record(usage: TokenUsage) {
    let _ = USAGE_COLLECTOR.try_with(|c| {
        c.lock().unwrap_or_else(|e| e.into_inner()).push(usage);
    });
}

/// Sum a `Vec<TokenUsage>` into a single aggregate.  Empty input returns
/// `TokenUsage::default()`.
pub fn aggregate(records: &[TokenUsage]) -> TokenUsage {
    records.iter().fold(TokenUsage::default(), |acc, u| acc.add(u))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_sums_fields_and_picks_longer_model() {
        let a = TokenUsage { prompt_tokens: 100, completion_tokens: 50, cached_tokens: 0,  model: "claude".into() };
        let b = TokenUsage { prompt_tokens:  20, completion_tokens: 10, cached_tokens: 5,  model: "claude-sonnet-4-6".into() };
        let s = a.add(&b);
        assert_eq!(s.prompt_tokens,     120);
        assert_eq!(s.completion_tokens, 60);
        assert_eq!(s.cached_tokens,     5);
        assert_eq!(s.model,             "claude-sonnet-4-6");
    }

    #[test]
    fn cost_uses_billed_minus_cached_for_input() {
        // 1M input tokens, 100k cached, 200k output, sonnet rates
        let u = TokenUsage {
            prompt_tokens:     1_000_000,
            completion_tokens:   200_000,
            cached_tokens:       100_000,
            model:               "claude-sonnet-4-6".into(),
        };
        // billed_input = 900k * $3/M = $2.70
        // cached       = 100k * $0.30/M = $0.03
        // output       = 200k * $15/M = $3.00
        // total        = $5.73
        let cost = u.cost_usd();
        assert!((cost - 5.73).abs() < 0.001, "cost was {cost}");
    }

    #[test]
    fn cost_zero_for_local_backend() {
        let u = TokenUsage {
            prompt_tokens: 500_000, completion_tokens: 100_000,
            cached_tokens: 0, model: "llama3.2".into(),
        };
        assert_eq!(u.cost_usd(), 0.0);
    }

    #[test]
    fn cost_unknown_model_uses_safe_default() {
        let u = TokenUsage {
            prompt_tokens: 1_000_000, completion_tokens: 0,
            cached_tokens: 0, model: "totally-made-up-vNext".into(),
        };
        // Default is sonnet rate ($3/M input)
        assert!((u.cost_usd() - 3.0).abs() < 0.001);
    }

    #[test]
    fn record_outside_scope_is_silent_noop() {
        // Should not panic.
        record(TokenUsage::default());
    }

    #[tokio::test]
    async fn with_recording_collects_calls_inside_scope() {
        let (out, recs) = with_recording(async {
            record(TokenUsage { prompt_tokens: 10, ..Default::default() });
            record(TokenUsage { prompt_tokens: 20, completion_tokens: 5, ..Default::default() });
            42
        }).await;
        assert_eq!(out, 42);
        assert_eq!(recs.len(), 2);
        let total = aggregate(&recs);
        assert_eq!(total.prompt_tokens, 30);
        assert_eq!(total.completion_tokens, 5);
    }

    #[tokio::test]
    async fn record_after_scope_returns_does_not_leak_to_outer() {
        let (_inner_recs, _) = ((), 0);
        let (_, recs_outer) = with_recording(async {
            record(TokenUsage { prompt_tokens: 1, ..Default::default() });
            with_recording(async {
                record(TokenUsage { prompt_tokens: 2, ..Default::default() });
            }).await;
            // The inner scope's record should not show up in the outer
            // collector here since the inner future already returned and
            // its task-local frame popped.  The outer scope only sees its
            // own direct record.
            record(TokenUsage { prompt_tokens: 3, ..Default::default() });
        }).await;
        let total = aggregate(&recs_outer);
        assert_eq!(total.prompt_tokens, 1 + 3); // not 6
    }
}
