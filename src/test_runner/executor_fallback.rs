//! Integration point for Open Claude fallback in test executor
//!
//! When AXTree-based browser control fails repeatedly, try Open Claude's
//! precise coordinate-based control as a fallback.
//!
//! ## Integration Path (Future PR)
//!
//! Currently this module provides the fallback infrastructure but is not yet
//! wired into the main executor loop. To enable fallback:
//!
//! 1. In `executor.rs::execute_test_tracked()`, add at line 128:
//!    ```rust
//!    let mut fallback_ctx = crate::test_runner::executor_fallback::AxtreeFallbackContext::new();
//!    ```
//!
//! 2. When an `ax_find` call returns "no element at viewport" or similar error,
//!    record it in `fallback_ctx`:
//!    ```rust
//!    if observation.contains("no element") {
//!        fallback_ctx.record_failure(&observation);
//!        if fallback_ctx.should_fallback() {
//!            // Try Open Claude as fallback (requires native host setup)
//!            match fallback_ctx.try_open_claude_fallback(prompt).await {
//!                Ok((x, y)) => {
//!                    // Execute fallback click at (x, y)
//!                    // Reset fallback_ctx on success
//!                }
//!                Err(e) => tracing::warn!("Open Claude fallback failed: {}", e),
//!            }
//!        }
//!    }
//!    ```
//!
//! 3. When any action succeeds, reset the failure counter:
//!    ```rust
//!    if action_succeeded {
//!        fallback_ctx.reset();
//!    }
//!    ```

use crate::open_claude_client::{OpenClaudeClient, OpenClaudeConfig};

/// Fallback context: tracks failed ax_find attempts
#[derive(Debug, Clone, Default)]
pub struct AxtreeFallbackContext {
    pub consecutive_failures: u32,
    pub failure_threshold: u32,  // e.g., 5 consecutive failures → try Open Claude
    pub last_failure: Option<String>,
    pub open_claude_client: Option<OpenClaudeClient>,
}

impl AxtreeFallbackContext {
    pub fn new() -> Self {
        let client = OpenClaudeClient::new(OpenClaudeConfig::default());
        Self {
            consecutive_failures: 0,
            failure_threshold: 5,
            last_failure: None,
            open_claude_client: Some(client),
        }
    }

    /// Called when ax_find fails
    pub fn record_failure(&mut self, reason: &str) {
        self.consecutive_failures += 1;
        self.last_failure = Some(reason.to_string());
    }

    /// Called when any action succeeds
    pub fn reset(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Should we attempt Open Claude fallback?
    pub fn should_fallback(&self) -> bool {
        self.consecutive_failures >= self.failure_threshold
            && self.open_claude_client.is_some()
    }

    /// Try Open Claude as fallback
    pub async fn try_open_claude_fallback(
        &mut self,
        _prompt: &str,
    ) -> Result<(u32, u32), String> {
        // For now, just return a dummy coordinate
        // In production, would call: self.open_claude_client.computer_tool(prompt).await
        
        // TODO: Implement actual fallback call once Open Claude is available
        // let result = self.open_claude_client.as_ref().unwrap().computer_tool(prompt).await?;
        // Ok((result.x, result.y))
        
        // Stub: return error for now (Open Claude not yet connected in test)
        Err("Open Claude fallback not yet connected (development stub)".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fallback_threshold() {
        let mut ctx = AxtreeFallbackContext::new();
        ctx.failure_threshold = 3;

        assert!(!ctx.should_fallback());
        ctx.record_failure("test");
        assert!(!ctx.should_fallback());
        ctx.record_failure("test");
        assert!(!ctx.should_fallback());
        ctx.record_failure("test");
        assert!(ctx.should_fallback());
    }

    #[test]
    fn test_reset() {
        let mut ctx = AxtreeFallbackContext::new();
        ctx.consecutive_failures = 5;
        assert!(ctx.should_fallback());
        ctx.reset();
        assert!(!ctx.should_fallback());
    }
}
