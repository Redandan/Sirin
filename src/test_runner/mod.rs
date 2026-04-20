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
pub mod notify;

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
    /// Override LLM backend for this run.  See [`TestGoal::llm_backend`].
    /// Common value: `Some("claude_cli")` to use `claude -p` subprocess
    /// instead of the configured Gemini/HTTP backend.
    pub llm_backend: Option<String>,
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
    let result = executor::execute_test_tracked(&ctx_with_run, &test, run_id, None).await;

    // Triage non-passed results
    let (category, analysis, fix_triggered) = if matches!(result.status, TestStatus::Passed) {
        (None, None, false)
    } else {
        let outcome = triage::triage(ctx, &test, &result).await;
        let triggered = auto_fix && triage::trigger_auto_fix(&test, &result, &outcome, run_id);
        (
            Some(outcome.category.as_str().to_string()),
            Some(outcome.reason.clone()),
            triggered,
        )
    };

    // Persist to SQLite — goal + run_id stored so persist_adhoc_run
    // can recover after the in-memory state is pruned.
    let history_json = serde_json::to_string(&result.history).ok();
    let goal_json = serde_json::to_string(&test).ok();
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
        goal_json: goal_json.as_deref(),
        run_id,
        iterations: Some(result.iterations),
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
        llm_backend: req.llm_backend,
        success_criteria: req.success_criteria,
        tags: vec!["adhoc".into()],
        fixture: req.fixture,
        docs_refs: vec![],  // ad-hoc runs have no pre-defined required reading
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
            let result = executor::execute_test_tracked(&ctx, &test_clone, Some(&run_id_clone), None).await;

            // Persist to SQLite — ad-hoc runs are still worth recording
            let history_json = serde_json::to_string(&result.history).ok();
            let status_str = match result.status {
                TestStatus::Passed  => "passed",
                TestStatus::Failed  => "failed",
                TestStatus::Timeout => "timeout",
                TestStatus::Error   => "error",
            };
            // Persist the goal too so persist_adhoc_run can recover after
            // the in-memory run state is pruned (1 hour TTL).
            let goal_json = serde_json::to_string(&test_clone).ok();
            let _ = store::record_run(store::NewRun {
                test_id: &test_clone.id,
                started_at: &started,
                duration_ms: Some(result.duration_ms as i64),
                status: status_str,
                failure_category: None,
                ai_analysis: result.final_analysis.as_deref(),
                screenshot_path: result.screenshot_path.as_deref(),
                history_json: history_json.as_deref(),
                goal_json: goal_json.as_deref(),
                run_id: Some(&run_id_clone),
                iterations: Some(result.iterations),
            });

            runs::set_phase(&run_id_clone, runs::RunPhase::Complete(result));
        });
    });

    Ok(run_id)
}

/// Spawn N test runs in parallel, each on its own dedicated chrome tab
/// (`session_id = batch_<batch_id>_<idx>`).  `max_concurrency` caps the number
/// of tests running simultaneously via a [`tokio::sync::Semaphore`]; the rest
/// queue up and start as permits free.
///
/// Returns one `run_id` per test in the same order as the input — callers can
/// poll [`runs::get`] to track each independently.
///
/// Persistence and triage are skipped on the batch path (caller can re-run
/// individual tests via the regular `run_test` API if a failure needs auto-fix).
/// Results are still recorded to SQLite via [`store::record_run`].
pub fn spawn_batch_run(
    test_ids: Vec<String>,
    max_concurrency: usize,
) -> Result<Vec<String>, String> {
    if test_ids.is_empty() {
        return Err("test_ids is empty".into());
    }
    // Pre-validate everything before spawning anything — fail fast.
    for tid in &test_ids {
        if parser::find(tid).is_none() {
            return Err(format!("Test '{tid}' not found"));
        }
    }

    let batch_id = format!("batch_{}", chrono::Local::now().format("%Y%m%d_%H%M%S_%3f"));
    let mut run_ids: Vec<String> = Vec::with_capacity(test_ids.len());
    for tid in &test_ids {
        run_ids.push(runs::new_run(tid));
    }

    let cap = max_concurrency.max(1);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(cap));

    for (idx, (test_id, run_id)) in test_ids.iter().zip(run_ids.iter()).enumerate() {
        let sem_clone = sem.clone();
        let test_id_clone = test_id.clone();
        let run_id_clone = run_id.clone();
        let session_id = format!("{batch_id}_{idx:02}");

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
                // Wait for a slot — Semaphore::acquire is fair (FIFO).
                let _permit = match sem_clone.acquire().await {
                    Ok(p) => p,
                    Err(e) => {
                        runs::set_phase(&run_id_clone, runs::RunPhase::Error(
                            format!("semaphore closed: {e}")
                        ));
                        return;
                    }
                };

                let test = match parser::find(&test_id_clone) {
                    Some(t) => t,
                    None => {
                        runs::set_phase(&run_id_clone, runs::RunPhase::Error(
                            format!("test '{test_id_clone}' not found")
                        ));
                        return;
                    }
                };

                let tools = crate::adk::tool::default_tool_registry();
                let ctx = crate::adk::context::AgentContext::new("mcp_batch", tools)
                    .with_metadata("test_run_id", &run_id_clone);

                let started = chrono::Local::now().to_rfc3339();
                let result = executor::execute_test_tracked(
                    &ctx, &test, Some(&run_id_clone), Some(&session_id)
                ).await;

                // Persist to SQLite — same shape as spawn_run_async.
                let history_json = serde_json::to_string(&result.history).ok();
                let status_str = match result.status {
                    TestStatus::Passed  => "passed",
                    TestStatus::Failed  => "failed",
                    TestStatus::Timeout => "timeout",
                    TestStatus::Error   => "error",
                };
                let goal_json = serde_json::to_string(&test).ok();
                let _ = store::record_run(store::NewRun {
                    test_id: &test.id,
                    started_at: &started,
                    duration_ms: Some(result.duration_ms as i64),
                    status: status_str,
                    failure_category: None,
                    ai_analysis: result.final_analysis.as_deref(),
                    screenshot_path: result.screenshot_path.as_deref(),
                    history_json: history_json.as_deref(),
                    goal_json: goal_json.as_deref(),
                    run_id: Some(&run_id_clone),
                    iterations: Some(result.iterations),
                });

                runs::set_phase(&run_id_clone, runs::RunPhase::Complete(result));

                // Best-effort close the dedicated tab.  Browser actions hold
                // the session in `OnceLock<Mutex>` — release it so the next
                // batch doesn't accumulate ghost tabs.
                let sid = session_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = crate::browser::close_session(&sid);
                }).await;
            });
        });
    }

    Ok(run_ids)
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

    // Two-tier recovery: fast path (in-memory registry, set within the
    // last hour), slow path (SQLite test_runs row by run_id).  This
    // means an external AI can explore at 9am and persist at 5pm —
    // the in-memory state is gone but the YAML goal + iteration count
    // were committed to disk at run completion.
    let (goal, iterations_used, status_passed) = if let Some(state) = runs::get(&p.run_id) {
        // Fast path — read directly from in-memory state.
        let goal = state.test_goal.clone().ok_or_else(|| format!(
            "run_id '{}' has no stored TestGoal.  This usually means the run \
             was started before Sirin was upgraded to support persist — \
             re-run the exploration to capture the goal.",
            p.run_id
        ))?;
        let (iters, passed) = match &state.phase {
            runs::RunPhase::Complete(r) => (
                r.iterations,
                matches!(r.status, executor::TestStatus::Passed),
            ),
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
        if !passed {
            return Err(format!(
                "run '{}' did not pass — refusing to persist a regression \
                 test that would always fail.  If the goal is right but the \
                 page is buggy, fix the page first then re-run.",
                p.run_id
            ));
        }
        (goal, iters, true)
    } else {
        // Slow path — in-memory state pruned, but the SQLite row survives.
        let (goal_json, status, iters) = store::find_run_by_run_id(&p.run_id)
            .ok_or_else(|| format!(
                "run_id '{}' not found in either in-memory registry or SQLite \
                 history — either the run never happened, or it predates \
                 Sirin v0.4.0 (which added SQLite goal persistence).  Re-run \
                 the exploration to capture the goal.",
                p.run_id
            ))?;
        if status != "passed" {
            return Err(format!(
                "run '{}' has SQLite status='{}' — refusing to persist a regression \
                 test that would always fail",
                p.run_id, status
            ));
        }
        let goal: TestGoal = serde_json::from_str(&goal_json)
            .map_err(|e| format!("recovered goal_json failed to parse: {e}"))?;
        (goal, iters, true)
    };
    let _ = status_passed; // bound for clarity but the early returns made it redundant

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
        llm_backend: goal.llm_backend.clone(),
        success_criteria: goal.success_criteria.clone(),
        tags: tags.clone(),
        fixture: goal.fixture.clone(),
        docs_refs: goal.docs_refs.clone(),  // propagate required-reading from source run
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
            llm_backend: None,
            success_criteria: vec!["URL contains /dashboard".into()],
            tags: vec!["adhoc".into()],
            fixture: None,
            docs_refs: vec![],
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
            llm_backend: None,
            success_criteria: vec![],
            tags: vec![],
            fixture: None,
            docs_refs: vec![],
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
    fn recovers_pruned_run_from_sqlite() {
        // Simulate the "explored at 9am, came back at 5pm" scenario:
        // the in-memory run state was pruned, but the SQLite row still
        // has the goal_json + iterations.  persist_adhoc_run must
        // succeed via the slow-path recovery branch.
        let unique = chrono::Local::now().timestamp_nanos_opt().unwrap_or(0);
        let test_id = format!("persist_recovered_{unique}");
        let synthetic_run_id = format!("run_pruned_{unique}");
        let goal = TestGoal {
            id: format!("adhoc_{unique}"),
            name: "Ad-hoc: test recovery".into(),
            url: "https://example.com/recovery".into(),
            goal: "verify recovery flow".into(),
            max_iterations: 10,
            timeout_secs: 60,
            retry_on_parse_error: 3,
            locale: "en".into(),
            url_query: Default::default(),
            browser_headless: Some(true),
            llm_backend: None,
            success_criteria: vec!["Recovered OK".into()],
            tags: vec!["adhoc".into()],
            fixture: None,
            docs_refs: vec![],
        };
        // Insert directly into SQLite — simulates the row that
        // record_run wrote at the original run completion.
        let goal_json_str = serde_json::to_string(&goal).unwrap();
        store::record_run(store::NewRun {
            test_id: &goal.id,
            started_at: &chrono::Local::now().to_rfc3339(),
            duration_ms: Some(8_000),
            status: "passed",
            failure_category: None,
            ai_analysis: None,
            screenshot_path: None,
            history_json: None,
            goal_json: Some(&goal_json_str),
            run_id: Some(&synthetic_run_id),
            iterations: Some(6),
        }).unwrap();

        // NOTE: we deliberately do NOT call runs::new_run() — the in-memory
        // state is absent, so persist_adhoc_run must hit the SQLite fallback.
        let result = persist_adhoc_run(PersistAdhocParams {
            run_id: synthetic_run_id,
            test_id: test_id.clone(),
            name: None,
            tags: None,
            bump_iterations: true,
            overwrite: false,
        }).expect("recovery via SQLite must succeed");
        assert_eq!(result.test_id, test_id);
        assert_eq!(result.iterations_used, 6);

        // Round-trip the persisted YAML
        let loaded = parser::load_file(std::path::Path::new(&result.yaml_path))
            .expect("YAML parseable");
        assert_eq!(loaded.url, "https://example.com/recovery");
        assert_eq!(loaded.success_criteria, vec!["Recovered OK".to_string()]);
        // bump_iterations=true → max(used+5, original) = max(11, 10) = 11
        assert_eq!(loaded.max_iterations, 11);

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

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn rejects_empty_test_ids() {
        let result = spawn_batch_run(Vec::new(), 3);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn rejects_unknown_test_id() {
        // Even a single bogus id should abort the entire batch — fail fast.
        let result = spawn_batch_run(
            vec!["this_test_does_not_exist_xyz_999".into()],
            3,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("not found"), "err was: {err}");
    }

    #[test]
    fn aborts_when_any_id_unknown() {
        // wiki_smoke does exist (ships in config/tests), but the second is bogus.
        // The whole call must be rejected without spawning anything.
        let result = spawn_batch_run(
            vec!["wiki_smoke".into(), "totally_bogus_xyz_999".into()],
            3,
        );
        assert!(result.is_err(), "any unknown id must abort entire batch");
    }
}
