//! AI-driven browser test runner.
//!
//! Reads goals from `config/tests/*.yaml`, uses a ReAct loop to let an LLM
//! drive the `web_navigate` browser tool, records every run to SQLite,
//! classifies failures, and can auto-spawn Claude Code sessions to fix
//! root causes in `frontend` / `backend` repos.
//!
//! Submodules:
//! - [`parser`]   — YAML → [`TestGoal`]
//! - [`executor`] — ReAct loop, produces [`TestResult`]
//! - [`triage`]   — failure classification + auto-fix spawning
//! - [`store`]    — SQLite history + learned knowledge

pub mod parser;
pub mod executor;
pub mod triage;
pub mod store;

pub use parser::TestGoal;
pub use executor::{TestResult, TestStatus};
#[allow(unused_imports)]
pub use executor::TestStep;
#[allow(unused_imports)]
pub use triage::{FailureCategory, TriageOutcome};

/// Run a single test by id, record the result, and optionally auto-fix.
///
/// Returns the full [`TestResult`].  Pass `auto_fix=true` to spawn a
/// Claude session for UI/API bugs in the background.
pub async fn run_test(
    ctx: &crate::adk::context::AgentContext,
    test_id: &str,
    auto_fix: bool,
) -> Result<TestResult, String> {
    let test = parser::find(test_id)
        .ok_or_else(|| format!("Test '{test_id}' not found in config/tests/"))?;

    let started = chrono::Local::now().to_rfc3339();
    let result = executor::execute_test(ctx, &test).await;

    // Triage non-passed results
    let (category, analysis, fix_triggered) = if matches!(result.status, TestStatus::Passed) {
        (None, None, false)
    } else {
        let outcome = triage::triage(ctx, &test, &result).await;
        let triggered = auto_fix && triage::trigger_auto_fix(&test, &result, &outcome);
        (
            Some(outcome.category.as_str().to_string()),
            Some(outcome.reason.clone()),
            triggered,
        )
    };

    // Persist to SQLite
    let history_json = serde_json::to_string(&result.history).ok();
    let _ = store::record_run(store::NewRun {
        test_id: &test.id,
        started_at: &started,
        duration_ms: Some(result.duration_ms as i64),
        status: match result.status {
            TestStatus::Passed  => "passed",
            TestStatus::Failed  => "failed",
            TestStatus::Timeout => "timeout",
            TestStatus::Error   => "error",
        },
        failure_category: category.as_deref(),
        ai_analysis: analysis.as_deref(),
        screenshot_path: result.screenshot_path.as_deref(),
        history_json: history_json.as_deref(),
    });

    if fix_triggered {
        tracing::info!("[test_runner] auto_fix spawned for test '{}'", test.id);
    }

    Ok(result)
}

/// Run all tests matching the optional tag filter.
pub async fn run_all(
    ctx: &crate::adk::context::AgentContext,
    tag: Option<&str>,
    auto_fix: bool,
) -> Vec<TestResult> {
    let tests = parser::load_all();
    let mut out = Vec::with_capacity(tests.len());
    for test in tests {
        if let Some(t) = tag {
            if !test.tags.iter().any(|x| x == t) { continue; }
        }
        match run_test(ctx, &test.id, auto_fix).await {
            Ok(r) => out.push(r),
            Err(e) => tracing::warn!("run_test '{}' failed: {e}", test.id),
        }
    }
    out
}

/// List all tests from `config/tests/`.
pub fn list_tests() -> Vec<TestGoal> {
    parser::load_all()
}
