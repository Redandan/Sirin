//! Open Claude-based test executor — uses computer tool for all UI interactions
//!
//! This is the new primary executor that replaces the AXTree-based approach.
//! Instead of failing on Canvas apps, it uses Open Claude's vision + pixel-level control.

use serde_json::json;
use super::parser::TestGoal;
use super::executor::{TestResult, TestStatus, TestStep};

/// Execute a test using Open Claude computer tool for browser control.
///
/// Flow:
/// 1. Navigate to URL
/// 2. For each iteration:
///    a. Take screenshot
///    b. Call Open Claude: "Based on this screenshot, what should I do to achieve: {goal}"
///    c. Claude returns: screenshot analysis + suggested action
///    d. Execute the action via click_point/type/etc
///    e. Validate state change
///    f. Repeat until goal achieved or max iterations
pub async fn execute_test_open_claude(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
    run_id: Option<&str>,
) -> TestResult {
    use crate::test_runner::runs;

    let started = std::time::Instant::now();
    let mut history: Vec<TestStep> = Vec::new();

    if let Some(rid) = run_id {
        runs::set_phase(rid, runs::RunPhase::Running { step: 0, current_action: "goto".into() });
    }

    // Browser setup
    let want_headless = test.browser_headless.unwrap_or_else(crate::browser::default_headless);
    if let Err(e) = tokio::task::spawn_blocking(move || {
        crate::browser::set_test_headless_mode(want_headless);
        crate::browser::ensure_open(want_headless)
    })
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))
        .and_then(|r| r)
    {
        return TestResult {
            test_id: test.id.clone(),
            status: TestStatus::Error,
            iterations: 0,
            duration_ms: started.elapsed().as_millis() as u64,
            error_message: Some(format!("browser launch failed: {e}")),
            screenshot_path: None,
            screenshot_error: None,
            history,
            final_analysis: None,
        };
    }

    // Navigate to test URL
    let nav_url = test.full_url();
    let nav_input = json!({ "action": "goto", "target": &nav_url });
    if let Err(e) = ctx.call_tool("web_navigate", nav_input).await {
        return TestResult {
            test_id: test.id.clone(),
            status: TestStatus::Error,
            iterations: 0,
            duration_ms: started.elapsed().as_millis() as u64,
            error_message: Some(format!("navigate failed: {e}")),
            screenshot_path: None,
            screenshot_error: None,
            history,
            final_analysis: None,
        };
    }

    // Install capture
    let cap_input = json!({ "action": "install_capture" });
    let _ = ctx.call_tool("web_navigate", cap_input).await;

    // Main execution loop using Open Claude
    let max_iter = test.max_iterations.max(1);
    let deadline = started + std::time::Duration::from_secs(test.timeout_secs.max(10));

    for iteration in 0..max_iter {
        if std::time::Instant::now() >= deadline {
            return TestResult {
                test_id: test.id.clone(),
                status: TestStatus::Timeout,
                iterations: iteration,
                duration_ms: started.elapsed().as_millis() as u64,
                error_message: Some("timeout".to_string()),
                screenshot_path: None,
                screenshot_error: None,
                history,
                final_analysis: None,
            };
        }

        // Step 1: Take screenshot for Open Claude analysis
        let ss_input = json!({ "action": "screenshot" });
        let _screenshot_result = match ctx.call_tool("web_navigate", ss_input).await {
            Ok(val) => val,
            Err(e) => {
                history.push(TestStep {
                    thought: "Failed to take screenshot".to_string(),
                    action: json!({"action": "screenshot", "error": e.to_string()}),
                    observation: format!("Screenshot error: {e}"),
                });
                continue;
            }
        };

        // Step 2: Call Open Claude with screenshot
        // TODO: Call ctx's LLM with screenshot + goal
        // This would require access to the LLM service
        // For now, we'll use a placeholder that shows the integration point
        
        let thought = format!(
            "Iteration {}: Analyzing current page state with Open Claude to achieve: {}",
            iteration + 1,
            test.goal.lines().next().unwrap_or("(no goal)")
        );

        history.push(TestStep {
            thought,
            action: json!({"action": "screenshot_analyze", "target": &test.goal}),
            observation: "Open Claude would analyze screenshot here".to_string(),
        });

        // Step 3-4: (Placeholder for Open Claude action execution)
        // In real implementation, Open Claude returns coordinates + action type
        // Then we execute: click_point, type, scroll, etc.

        if let Some(rid) = run_id {
            runs::push_observation(
                rid,
                format!("Iter {}: Open Claude analysis (placeholder)", iteration + 1),
            );
        }
    }

    // Success criteria evaluation would go here
    // For now, stub it as passing since this is integration demo
    TestResult {
        test_id: test.id.clone(),
        status: TestStatus::Failed,
        iterations: max_iter,
        duration_ms: started.elapsed().as_millis() as u64,
        error_message: Some("Open Claude executor not yet fully integrated".to_string()),
        screenshot_path: None,
        screenshot_error: None,
        history,
        final_analysis: Some("Executor integration in progress".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_integration_placeholder() {
        // Placeholder test to ensure module compiles
        assert!(true);
    }
}
