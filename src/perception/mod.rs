//! Perception layer — how the ReAct loop "sees" the page.
//!
//! Sirin historically fed the LLM a truncated string observation (AX tree
//! dump, tool result, error text).  This is fine for classic DOM pages but
//! collapses on Flutter CanvasKit / WebGL where the AX tree is unreliable
//! and the page text is missing entirely.
//!
//! This module introduces an alternative: give the LLM a **screenshot** as
//! the primary observation, and demote the AX tree to a lookup tool that
//! the LLM can call explicitly when it wants exact text.  This mirrors how
//! Open Claude and Anthropic's computer-use tool operate.
//!
//! Modes (chosen per test via the `perception` YAML field):
//!   - `text`   — legacy path, no screenshot, LLM sees previous observations only
//!   - `vision` — always screenshot + vision LLM call
//!   - `auto`   — detect canvas; screenshot only when canvas is present
//!
//! Default is `text` — zero behavioural change for existing tests.

pub mod canvas_detect;
pub mod capture;
pub mod ocr;

use serde::{Deserialize, Serialize};

/// How the executor should observe the page before each LLM turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PerceptionMode {
    /// Legacy text-only observation.  No screenshot, no vision LLM call.
    #[default]
    Text,
    /// Always attach a screenshot to the prompt and call the vision LLM.
    Vision,
    /// Detect canvas (Flutter / WebGL) at runtime; use Vision if detected,
    /// otherwise Text.
    Auto,
}

impl PerceptionMode {
    /// Resolve `Auto` to a concrete mode based on a canvas-detection result.
    /// `Text` and `Vision` are returned unchanged.
    pub fn resolve(self, canvas_detected: bool) -> PerceptionMode {
        match self {
            PerceptionMode::Auto if canvas_detected => PerceptionMode::Vision,
            PerceptionMode::Auto => PerceptionMode::Text,
            m => m,
        }
    }
}

/// A single observation captured before an LLM turn.
///
/// Always carries a cheap textual `summary` (URL + title + canvas flag) so
/// the LLM has baseline context even in pure-vision mode.  The screenshot
/// is only populated when the resolved mode is `Vision`.
#[derive(Debug, Clone)]
pub struct PagePerception {
    pub url: String,
    pub title: String,
    pub canvas_detected: bool,
    /// One-line human-readable summary for inclusion in the text prompt.
    pub summary: String,
    /// Base64-encoded PNG of the viewport, present only for Vision mode.
    pub screenshot_b64: Option<String>,
    /// The mode that was actually used (after Auto resolution).
    pub resolved_mode: PerceptionMode,
}

impl PagePerception {
    pub fn empty(resolved_mode: PerceptionMode) -> Self {
        Self {
            url: String::new(),
            title: String::new(),
            canvas_detected: false,
            summary: String::from("(perception unavailable)"),
            screenshot_b64: None,
            resolved_mode,
        }
    }
}

/// Capture a perception snapshot of the currently-open browser page.
///
/// This function never fails in a way that breaks the caller — on error it
/// returns a `PagePerception::empty(...)` so the executor can fall through
/// to legacy behaviour instead of aborting the whole run.
pub async fn perceive(
    ctx: &crate::adk::context::AgentContext,
    mode: PerceptionMode,
) -> PagePerception {
    // Fast path: legacy text-only mode incurs no perception overhead —
    // the executor goes straight to the old prompt-building path.
    if matches!(mode, PerceptionMode::Text) {
        return PagePerception::empty(PerceptionMode::Text);
    }

    // Step 1: cheap context (URL + title + canvas flag) via one JS eval.
    let probe = canvas_detect::probe_page(ctx).await;

    let resolved = mode.resolve(probe.canvas_detected);

    let summary = format!(
        "url={} title={:?} canvas={}",
        probe.url, probe.title, probe.canvas_detected
    );

    let screenshot_b64 = if matches!(resolved, PerceptionMode::Vision) {
        match capture::screenshot_b64().await {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!("[perception] screenshot_b64 failed: {e}");
                None
            }
        }
    } else {
        None
    };

    PagePerception {
        url: probe.url,
        title: probe.title,
        canvas_detected: probe.canvas_detected,
        summary,
        screenshot_b64,
        resolved_mode: resolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_resolve_auto() {
        assert_eq!(PerceptionMode::Auto.resolve(true), PerceptionMode::Vision);
        assert_eq!(PerceptionMode::Auto.resolve(false), PerceptionMode::Text);
    }

    #[test]
    fn mode_resolve_explicit_unchanged() {
        assert_eq!(PerceptionMode::Vision.resolve(false), PerceptionMode::Vision);
        assert_eq!(PerceptionMode::Text.resolve(true), PerceptionMode::Text);
    }

    #[test]
    fn empty_carries_resolved_mode() {
        let p = PagePerception::empty(PerceptionMode::Vision);
        assert_eq!(p.resolved_mode, PerceptionMode::Vision);
        assert!(p.screenshot_b64.is_none());
    }
}
