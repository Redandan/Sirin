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
pub mod runs;
pub mod i18n;

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
    // Always allocate a run_id — enables expand_observation inside the ReAct
    // loop even when caller didn't explicitly request async tracking.
    let run_id = runs::new_run(test_id);
    let result = run_test_with_run_id(ctx, test_id, auto_fix, Some(&run_id)).await;
    // Mark as complete in registry so MCP pollers see a terminal state.
    match &result {
        Ok(r) => runs::set_phase(&run_id, runs::RunPhase::Complete(r.clone())),
        Err(e) => runs::set_phase(&run_id, runs::RunPhase::Error(e.clone())),
    }
    result
}

/// Internal variant that accepts a pre-allocated run_id for async tracking.
async fn run_test_with_run_id(
    ctx: &crate::adk::context::AgentContext,
    test_id: &str,
    auto_fix: bool,
    run_id: Option<&str>,
) -> Result<TestResult, String> {
    let test = parser::find(test_id)
        .ok_or_else(|| format!("Test '{test_id}' not found in config/tests/"))?;

    let started = chrono::Local::now().to_rfc3339();
    // Inject run_id into context metadata so web_navigate tool actions (e.g.
    // expand_observation) can find the current run.
    let ctx_with_run = if let Some(rid) = run_id {
        ctx.clone().with_metadata("test_run_id", rid)
    } else {
        ctx.clone()
    };
    let result = executor::execute_test_tracked(&ctx_with_run, &test, run_id).await;

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

/// Spawn an async test run in the background.  Returns the `run_id`
/// immediately; poll `runs::get(run_id)` to check status.
///
/// Used by the MCP `run_test_async` endpoint so external callers aren't
/// blocked by the 2-minute test execution.
pub fn spawn_run_async(test_id: String, auto_fix: bool) -> Result<String, String> {
    // Validate test exists before spawning
    if parser::find(&test_id).is_none() {
        return Err(format!("Test '{test_id}' not found"));
    }

    let run_id = runs::new_run(&test_id);
    let run_id_clone = run_id.clone();
    let test_id_clone = test_id.clone();

    // Spawn a dedicated tokio runtime so this survives the caller's scope
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                runs::set_phase(&run_id_clone, runs::RunPhase::Error(
                    format!("failed to create runtime: {e}")
                ));
                return;
            }
        };
        rt.block_on(async {
            let tools = crate::adk::tool::default_tool_registry();
            let ctx = crate::adk::context::AgentContext::new("mcp_async", tools)
                .with_metadata("run_id", &run_id_clone);

            match run_test_with_run_id(&ctx, &test_id_clone, auto_fix, Some(&run_id_clone)).await {
                Ok(r) => runs::set_phase(&run_id_clone, runs::RunPhase::Complete(r)),
                Err(e) => runs::set_phase(&run_id_clone, runs::RunPhase::Error(e)),
            }
        });
    });

    Ok(run_id)
}

/// Spawn an ad-hoc test (no YAML file) by providing the goal in-line.
/// Returns run_id for polling.  The synthetic test_id is `adhoc_<timestamp>`.
///
/// This unblocks external callers who want to test a URL without first
/// writing a YAML goal definition.
pub fn spawn_adhoc_run(
    url: String,
    goal: String,
    success_criteria: Vec<String>,
    locale: Option<String>,
    max_iterations: Option<u32>,
    timeout_secs: Option<u64>,
    browser_headless: Option<bool>,
    fixture: Option<crate::test_runner::parser::Fixture>,
) -> Result<String, String> {
    if url.trim().is_empty() {
        return Err("url is required".into());
    }
    if goal.trim().is_empty() {
        return Err("goal is required".into());
    }

    let test_id = format!("adhoc_{}", chrono::Local::now().format("%Y%m%d_%H%M%S_%3f"));
    let test = TestGoal {
        id: test_id.clone(),
        name: format!("Ad-hoc: {}", goal.chars().take(40).collect::<String>()),
        url,
        goal,
        max_iterations: max_iterations.unwrap_or(15),
        timeout_secs: timeout_secs.unwrap_or(120),
        retry_on_parse_error: 3,
        locale: locale.unwrap_or_else(|| "zh-TW".into()),
        url_query: Default::default(),
        browser_headless,
        success_criteria,
        tags: vec!["adhoc".into()],
        fixture,
    };

    let run_id = runs::new_run(&test_id);
    let run_id_clone = run_id.clone();
    let test_clone = test.clone();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                runs::set_phase(&run_id_clone, runs::RunPhase::Error(
                    format!("failed to create runtime: {e}")
                ));
                return;
            }
        };
        rt.block_on(async {
            let tools = crate::adk::tool::default_tool_registry();
            let ctx = crate::adk::context::AgentContext::new("mcp_adhoc", tools)
                .with_metadata("test_run_id", &run_id_clone);

            let started = chrono::Local::now().to_rfc3339();
            let result = executor::execute_test_tracked(&ctx, &test_clone, Some(&run_id_clone)).await;

            // Persist to SQLite — ad-hoc runs are still worth recording
            let history_json = serde_json::to_string(&result.history).ok();
            let status_str = match result.status {
                TestStatus::Passed  => "passed",
                TestStatus::Failed  => "failed",
                TestStatus::Timeout => "timeout",
                TestStatus::Error   => "error",
            };
            let _ = store::record_run(store::NewRun {
                test_id: &test_clone.id,
                started_at: &started,
                duration_ms: Some(result.duration_ms as i64),
                status: status_str,
                failure_category: None,
                ai_analysis: result.final_analysis.as_deref(),
                screenshot_path: result.screenshot_path.as_deref(),
                history_json: history_json.as_deref(),
            });

            runs::set_phase(&run_id_clone, runs::RunPhase::Complete(result));
        });
    });

    Ok(run_id)
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
