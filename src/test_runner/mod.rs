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

pub use parser::{TestGoal, Fixture};
pub use executor::{TestResult, TestStatus};
#[allow(unused_imports)]
pub use executor::TestStep;
#[allow(unused_imports)]
pub use triage::{FailureCategory, TriageOutcome};

/// Parameters for an ad-hoc (non-YAML) test run.  Passed to [`spawn_adhoc_run`].
///
/// Only `url` and `goal` are required; all other fields fall back to sensible defaults.
#[derive(Default)]
pub struct AdhocRunRequest {
    pub url: String,
    pub goal: String,
    pub success_criteria: Vec<String>,
    pub locale: Option<String>,
    pub max_iterations: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub browser_headless: Option<bool>,
    pub fixture: Option<Fixture>,
}

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

    // Attach goal to the run registry so other tools (e.g. persist_adhoc_run)
    // can look it up later.  Cheap clone — TestGoal is plain data.
    if let Some(rid) = run_id {
        runs::set_goal(rid, test.clone());
    }

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
pub fn spawn_adhoc_run(req: AdhocRunRequest) -> Result<String, String> {
    if req.url.trim().is_empty() {
        return Err("url is required".into());
    }
    if req.goal.trim().is_empty() {
        return Err("goal is required".into());
    }

    let test_id = format!("adhoc_{}", chrono::Local::now().format("%Y%m%d_%H%M%S_%3f"));
    let test = TestGoal {
        id: test_id.clone(),
        name: format!("Ad-hoc: {}", req.goal.chars().take(40).collect::<String>()),
        url: req.url,
        goal: req.goal,
        max_iterations: req.max_iterations.unwrap_or(15),
        timeout_secs: req.timeout_secs.unwrap_or(120),
        retry_on_parse_error: 3,
        locale: req.locale.unwrap_or_else(|| "zh-TW".into()),
        url_query: Default::default(),
        browser_headless: req.browser_headless,
        success_criteria: req.success_criteria,
        tags: vec!["adhoc".into()],
        fixture: req.fixture,
    };

    let run_id = runs::new_run(&test_id);
    // Attach the synthetic TestGoal to the run state so persist_adhoc_run
    // can later promote a successful exploration to a permanent YAML test.
    runs::set_goal(&run_id, test.clone());
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

// ── persist_adhoc_run ────────────────────────────────────────────────────────

/// Parameters for [`persist_adhoc_run`] — the bridge between ad-hoc
/// exploration and a permanent YAML regression test.
pub struct PersistAdhocParams {
    /// `run_id` returned by `spawn_adhoc_run` (or any `spawn_run_async`).
    pub run_id: String,
    /// New permanent test_id.  Becomes both the `id` field in the YAML
    /// and the filename (`config/tests/<test_id>.yaml`).  Must match
    /// `[a-z0-9_]+` so the filename is portable across OSes.
    pub test_id: String,
    /// Optional human-readable name; falls back to the original `name`.
    pub name: Option<String>,
    /// Tags to attach.  The synthetic `"adhoc"` tag is replaced with
    /// `"adhoc-derived"` so list_tests filters can distinguish persisted
    /// from in-flight ad-hoc runs.
    pub tags: Option<Vec<String>>,
    /// Append the iteration count + 5 to the original `max_iterations`
    /// to give regression runs slack for natural variance.  Default true.
    pub bump_iterations: bool,
    /// Refuse to overwrite an existing file unless this is true.
    pub overwrite: bool,
}

/// Result of a successful [`persist_adhoc_run`] call.
#[derive(Debug, serde::Serialize)]
pub struct PersistAdhocResult {
    pub test_id: String,
    pub yaml_path: String,
    pub iterations_used: u32,
    pub criteria_count: usize,
    pub tags: Vec<String>,
}

/// Promote a successful ad-hoc run to a permanent YAML test.
///
/// **Workflow:**
/// 1. External AI calls `run_adhoc_test(url, goal, ...)` — explores ad-hoc
/// 2. Run completes with `status=passed`
/// 3. AI calls `persist_adhoc_run(run_id, test_id="login_flow")`
/// 4. Sirin writes `config/tests/login_flow.yaml` carrying over goal,
///    success_criteria, locale, headless flag, url_query
/// 5. Future `run_test login_flow` works as a regression test
///
/// **Validation rules (returns Err on violation):**
/// - `test_id` must match `[a-z0-9_]+` (filename safe, lowercase)
/// - run must exist in the in-memory registry (not pruned)
/// - run must be `Complete(Passed)` — refuse to persist failed/in-flight runs
/// - file must not exist unless `overwrite=true`
pub fn persist_adhoc_run(p: PersistAdhocParams) -> Result<PersistAdhocResult, String> {
    // Validate test_id format up front — catches typos before we touch
    // the filesystem.  Mirrors the YAML test_id convention used in the
    // existing config/tests/ examples.
    if p.test_id.is_empty() {
        return Err("test_id must not be empty".into());
    }
    if !p.test_id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Err(format!(
            "test_id '{}' invalid — must match [a-z0-9_]+ (lowercase, digits, underscore)",
            p.test_id
        ));
    }
    // Block path traversal and YAML id collisions with reserved prefixes.
    if p.test_id.starts_with("adhoc_") {
        return Err("test_id must not start with 'adhoc_' — that prefix is reserved \
                    for in-flight runs.  Use a permanent name like 'login_flow'".into());
    }

    // Recover the run state.  If pruned (older than 1 hour), we can't
    // reconstruct the goal — surface that clearly so the caller can
    // re-run rather than silently writing a half-empty YAML.
    let state = runs::get(&p.run_id)
        .ok_or_else(|| format!(
            "run_id '{}' not found in registry — runs are pruned after 1 hour.  \
             Re-run the exploration if you want to persist it.",
            p.run_id
        ))?;

    // Goal storage was added in this same change; for runs spawned before
    // restart this can be None.  Be explicit about why.
    let goal = state.test_goal.clone()
        .ok_or_else(|| format!(
            "run_id '{}' has no stored TestGoal.  This usually means the run \
             was started before Sirin was upgraded to support persist — \
             re-run the exploration to capture the goal.",
            p.run_id
        ))?;

    // Refuse to persist incomplete or failed runs.  Persisting a failed
    // exploration would create a broken regression test that always fails;
    // not what the caller wants.
    let (iterations_used, status) = match &state.phase {
        runs::RunPhase::Complete(r) => (r.iterations, r.status.clone()),
        runs::RunPhase::Queued => return Err(format!(
            "run '{}' is still queued — wait for completion before persisting", p.run_id
        )),
        runs::RunPhase::Running { step, .. } => return Err(format!(
            "run '{}' is still running (step {step}) — wait for completion", p.run_id
        )),
        runs::RunPhase::Error(e) => return Err(format!(
            "run '{}' errored — refusing to persist a broken test: {e}", p.run_id
        )),
    };

    if !matches!(status, executor::TestStatus::Passed) {
        return Err(format!(
            "run '{}' did not pass (status={:?}) — refusing to persist a regression \
             test that would always fail.  If the goal is right but the page is buggy, \
             fix the page first then re-run.",
            p.run_id, status
        ));
    }

    // Build the permanent TestGoal.  Most fields carry over verbatim — the
    // only mutations are:
    //   - id  → caller-supplied permanent name
    //   - name → caller override or original
    //   - max_iterations → bumped for slack (regression runs encounter
    //     more variance than the original exploration did)
    //   - tags → "adhoc" stripped, "adhoc-derived" added (or caller override)
    let mut tags = p.tags.unwrap_or_else(|| {
        let mut t: Vec<String> = goal.tags.iter()
            .filter(|s| s.as_str() != "adhoc")
            .cloned()
            .collect();
        if !t.iter().any(|s| s == "adhoc-derived") {
            t.push("adhoc-derived".into());
        }
        t
    });
    tags.sort();
    tags.dedup();

    let max_iter = if p.bump_iterations {
        iterations_used.saturating_add(5).max(goal.max_iterations)
    } else {
        goal.max_iterations
    };

    let permanent = TestGoal {
        id: p.test_id.clone(),
        name: p.name.unwrap_or_else(|| {
            // The synthetic ad-hoc name is "Ad-hoc: <40-char goal preview>" —
            // strip the prefix when promoting so persisted YAML reads naturally.
            goal.name.strip_prefix("Ad-hoc: ").unwrap_or(&goal.name).to_string()
        }),
        url: goal.url.clone(),
        goal: goal.goal.clone(),
        max_iterations: max_iter,
        timeout_secs: goal.timeout_secs,
        retry_on_parse_error: goal.retry_on_parse_error,
        locale: goal.locale.clone(),
        url_query: goal.url_query.clone(),
        browser_headless: goal.browser_headless,
        success_criteria: goal.success_criteria.clone(),
        tags: tags.clone(),
        fixture: goal.fixture.clone(),
    };

    // Serialize and write.  serde_yaml uses 2-space indent and never
    // emits surprises for our simple struct — round-trip via parse_yaml
    // unit test catches any future field that doesn't survive.
    let yaml = serde_yaml::to_string(&permanent)
        .map_err(|e| format!("YAML serialization failed: {e}"))?;

    let dir = crate::platform::config_dir().join("tests");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create_dir_all {dir:?}: {e}"))?;
    let path = dir.join(format!("{}.yaml", p.test_id));

    if path.exists() && !p.overwrite {
        return Err(format!(
            "{} already exists — pass overwrite=true to replace it, or pick a different test_id",
            path.display()
        ));
    }

    std::fs::write(&path, &yaml)
        .map_err(|e| format!("write {path:?}: {e}"))?;

    tracing::info!(target: "sirin",
        "[test_runner] persisted ad-hoc run '{}' as test '{}' at {}",
        p.run_id, p.test_id, path.display()
    );

    Ok(PersistAdhocResult {
        test_id: p.test_id,
        yaml_path: path.to_string_lossy().into_owned(),
        iterations_used,
        criteria_count: permanent.success_criteria.len(),
        tags,
    })
}

#[cfg(test)]
mod persist_tests {
    use super::*;

    fn fake_passed_run(test_id: &str) -> String {
        let goal = TestGoal {
            id: test_id.to_string(),
            name: "Ad-hoc: explore login form".into(),
            url: "https://example.com/login".into(),
            goal: "fill email and submit".into(),
            max_iterations: 10,
            timeout_secs: 60,
            retry_on_parse_error: 3,
            locale: "en".into(),
            url_query: Default::default(),
            browser_headless: Some(false),
            success_criteria: vec!["URL contains /dashboard".into()],
            tags: vec!["adhoc".into()],
            fixture: None,
        };
        let run_id = runs::new_run(test_id);
        runs::set_goal(&run_id, goal.clone());
        // Synthesize a TestResult that says Passed
        let fake_result = executor::TestResult {
            test_id: test_id.to_string(),
            status: executor::TestStatus::Passed,
            iterations: 4,
            duration_ms: 12_000,
            history: Vec::new(),
            error_message: None,
            final_analysis: None,
            screenshot_path: None,
            screenshot_error: None,
        };
        runs::set_phase(&run_id, runs::RunPhase::Complete(fake_result));
        run_id
    }

    #[test]
    fn rejects_invalid_test_id() {
        let run_id = fake_passed_run("adhoc_99");
        let result = persist_adhoc_run(PersistAdhocParams {
            run_id,
            test_id: "Login-Flow".into(), // uppercase + hyphen → invalid
            name: None,
            tags: None,
            bump_iterations: true,
            overwrite: false,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid"));
    }

    #[test]
    fn rejects_adhoc_prefix_test_id() {
        let run_id = fake_passed_run("adhoc_99");
        let result = persist_adhoc_run(PersistAdhocParams {
            run_id,
            test_id: "adhoc_login".into(),
            name: None,
            tags: None,
            bump_iterations: true,
            overwrite: false,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("reserved"));
    }

    #[test]
    fn rejects_unknown_run_id() {
        let result = persist_adhoc_run(PersistAdhocParams {
            run_id: "does_not_exist".into(),
            test_id: "login_flow".into(),
            name: None,
            tags: None,
            bump_iterations: true,
            overwrite: false,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn rejects_running_run() {
        let run_id = runs::new_run("adhoc_running");
        runs::set_goal(&run_id, TestGoal {
            id: "adhoc_running".into(),
            name: "x".into(),
            url: "https://x".into(),
            goal: "y".into(),
            max_iterations: 10,
            timeout_secs: 60,
            retry_on_parse_error: 3,
            locale: "en".into(),
            url_query: Default::default(),
            browser_headless: None,
            success_criteria: vec![],
            tags: vec![],
            fixture: None,
        });
        runs::set_phase(&run_id, runs::RunPhase::Running {
            step: 2,
            current_action: "click".into(),
        });
        let result = persist_adhoc_run(PersistAdhocParams {
            run_id,
            test_id: "login_flow".into(),
            name: None,
            tags: None,
            bump_iterations: true,
            overwrite: false,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("still running"));
    }

    #[test]
    fn writes_yaml_and_carries_over_fields() {
        // Use a unique test_id per test invocation to avoid clobbering
        // existing config/tests/ entries when run alongside other tests.
        let unique_id = format!("persist_unit_{}", chrono::Local::now().format("%H%M%S%3f"));
        let run_id = fake_passed_run("adhoc_for_write");
        let result = persist_adhoc_run(PersistAdhocParams {
            run_id,
            test_id: unique_id.clone(),
            name: Some("Login Flow Regression".into()),
            tags: Some(vec!["smoke".into(), "auth".into()]),
            bump_iterations: true,
            overwrite: false,
        }).expect("persist should succeed");

        assert_eq!(result.test_id, unique_id);
        assert_eq!(result.iterations_used, 4);
        assert_eq!(result.criteria_count, 1);
        assert!(result.tags.contains(&"auth".to_string()));

        // Round-trip the file: load it back via parser and verify fields survived.
        let loaded = parser::load_file(std::path::Path::new(&result.yaml_path))
            .expect("YAML must be parseable");
        assert_eq!(loaded.id, unique_id);
        assert_eq!(loaded.name, "Login Flow Regression");
        assert_eq!(loaded.url, "https://example.com/login");
        assert_eq!(loaded.locale, "en");
        assert_eq!(loaded.browser_headless, Some(false));
        assert_eq!(loaded.success_criteria, vec!["URL contains /dashboard".to_string()]);
        // Tags should be sorted + deduped + NOT include "adhoc"
        assert!(!loaded.tags.iter().any(|t| t == "adhoc"));
        // bump_iterations=true → max(used+5, original) = max(9, 10) = 10
        assert_eq!(loaded.max_iterations, 10);

        // Cleanup so we don't pollute config/tests/
        let _ = std::fs::remove_file(&result.yaml_path);
    }

    #[test]
    fn refuses_to_overwrite_without_flag() {
        let unique_id = format!("persist_overwrite_{}", chrono::Local::now().format("%H%M%S%3f"));

        // First write succeeds
        let run_id = fake_passed_run("adhoc_first");
        let first = persist_adhoc_run(PersistAdhocParams {
            run_id,
            test_id: unique_id.clone(),
            name: None,
            tags: None,
            bump_iterations: true,
            overwrite: false,
        }).expect("first write must succeed");

        // Second write to the same path without overwrite=true must fail
        let run_id2 = fake_passed_run("adhoc_second");
        let second = persist_adhoc_run(PersistAdhocParams {
            run_id: run_id2.clone(),
            test_id: unique_id.clone(),
            name: None,
            tags: None,
            bump_iterations: true,
            overwrite: false,
        });
        assert!(second.is_err());
        assert!(second.unwrap_err().contains("already exists"));

        // With overwrite=true, it should succeed
        let third = persist_adhoc_run(PersistAdhocParams {
            run_id: run_id2,
            test_id: unique_id.clone(),
            name: None,
            tags: None,
            bump_iterations: true,
            overwrite: true,
        });
        assert!(third.is_ok());

        // Cleanup
        let _ = std::fs::remove_file(&first.yaml_path);
    }
}
