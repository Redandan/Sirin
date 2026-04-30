//! In-memory async run registry.
//!
//! Tracks active test executions so external MCP callers can trigger
//! `run_test_async(id)` → get a `run_id` immediately → poll
//! `get_test_result(run_id)` without blocking.
//!
//! Also stores full-length observations and screenshot bytes keyed by
//! `run_id` for later retrieval (since observations get truncated in
//! the LLM loop).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::executor::{TestResult, TestStatus};
use super::parser::TestGoal;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum RunPhase {
    Queued,
    Running { step: u32, current_action: String },
    Complete(TestResult),
    Error(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunState {
    pub run_id: String,
    pub test_id: String,
    pub started_at: String,
    pub phase: RunPhase,
    /// Full (non-truncated) observation text per step index.
    pub full_observations: Vec<String>,
    /// Screenshot bytes keyed by step index (0 = initial/goto).
    /// None for runs without failure screenshots.
    pub screenshot_bytes: Option<Vec<u8>>,
    pub screenshot_error: Option<String>,
    /// The full TestGoal that drove this run.  Stored so that
    /// `persist_adhoc_run` can recover the goal/url/criteria of a
    /// successful exploration and write it out as a permanent YAML
    /// regression test.  Always populated when the executor starts a
    /// run; may be `None` for the brief window between `new_run` and
    /// the first `set_goal` call.
    pub test_goal: Option<TestGoal>,
    /// Screenshot hash → vision analysis result cache (P1.1 optimization).
    /// Stored as `HashMap<String, String>` where key=SHA256(png), value=analysis.
    /// Avoids re-analyzing identical screenshots via the vision LLM.
    /// Initialized empty; populated during screenshot_analyze actions.
    #[serde(skip)]
    pub screenshot_cache: std::collections::HashMap<String, String>,
    /// A11y tree auto-diff context (P1.2 optimization).
    /// Stores baseline tree and computes diffs for subsequent ax_tree calls.
    /// Avoids sending full multi-KB JSON trees for each step.
    #[serde(skip)]
    pub ax_diff_context: crate::test_runner::ax_diff_context::AxDiffContext,
    /// SoM (Set-of-Mark) label map: label_id → (x, y) coordinates (P1.1 vision).
    /// Populated when screenshot_analyze runs with AX tree available.
    /// Used to convert LLM's "click label 5" → actual pixel coordinates.
    #[serde(skip)]
    pub som_label_map: Option<crate::test_runner::som_renderer::SoMLabelMap>,
    /// Most recent AX tree nodes (for SoM preparation in vision branch).
    /// Updated whenever ax_tree is called; used to prepare SoM labels before vision LLM.
    #[serde(skip)]
    pub recent_ax_nodes: Option<Vec<serde_json::Value>>,
    /// Timestamp of the last phase update (set_phase call).  Used by
    /// `to_json` to expose `idle_secs` so callers can detect stuck runs
    /// where the test hasn't progressed for an unusually long time.
    /// Initialized to `Instant::now()` on first `new_run`; updated on
    /// every `set_phase` call.  #[serde(skip)] — not persistent.
    #[serde(skip, default = "std::time::Instant::now")]
    pub last_phase_updated_at: std::time::Instant,
}

// ── Registry singleton ───────────────────────────────────────────────────────

fn registry() -> &'static Mutex<HashMap<String, RunState>> {
    static REG: OnceLock<Mutex<HashMap<String, RunState>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── API ──────────────────────────────────────────────────────────────────────

/// Generate a unique run_id + insert an initial Queued state.
pub fn new_run(test_id: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let run_id = format!("run_{}_{seq}", chrono::Local::now().format("%Y%m%d_%H%M%S_%3f"));
    let state = RunState {
        run_id: run_id.clone(),
        test_id: test_id.to_string(),
        started_at: chrono::Local::now().to_rfc3339(),
        phase: RunPhase::Queued,
        full_observations: Vec::new(),
        screenshot_bytes: None,
        screenshot_error: None,
        test_goal: None,
        screenshot_cache: std::collections::HashMap::new(),
        ax_diff_context: crate::test_runner::ax_diff_context::AxDiffContext::new(),
        som_label_map: None,
        recent_ax_nodes: None,
        last_phase_updated_at: std::time::Instant::now(),
    };
    registry().lock().unwrap_or_else(|e| e.into_inner())
        .insert(run_id.clone(), state);
    prune_old_runs();
    run_id
}

/// Attach the TestGoal that drove this run.  Called by the executor /
/// `spawn_adhoc_run` so the run is fully self-describing for later
/// `persist_adhoc_run` calls.  No-op when `run_id` doesn't exist.
pub fn set_goal(run_id: &str, goal: TestGoal) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        s.test_goal = Some(goal);
    }
}

/// Get the TestGoal stored for `run_id`.  Returns `None` if the run
/// has been pruned, was never started, or `set_goal` was not called.
pub fn get_goal(run_id: &str) -> Option<TestGoal> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
        .get(run_id)?.test_goal.clone()
}

/// Update phase (called by executor as it progresses).
///
/// When transitioning to `Complete` with a non-passing status, fires a
/// fire-and-forget Telegram notification via [`super::notify::notify_failure`].
/// The notification is a no-op when `SIRIN_NOTIFY_BOT_TOKEN` / `SIRIN_NOTIFY_CHAT_ID`
/// are not set — callers never need to handle this.
///
/// Also fires a fire-and-forget KB write-back:
/// - **Failed/Timeout/Error** → `sirin-failure-{test_id}` capturing error context.
/// - **Passed** → `sirin-pass-{test_id}` capturing the successful action sequence
///   (selectors / flow patterns) so other tests can mine confirmed-working
///   navigation paths.  Idempotent topicKey lets KB's auto-versioning collapse
///   repeated CI passes — promote-to-confirmed is handled downstream by the
///   existing version>=3 logic (Issue #159).
pub fn set_phase(run_id: &str, phase: RunPhase) {
    // Trigger failure notification + KB write-back before storing (fire-and-forget).
    if let RunPhase::Complete(ref r) = phase {
        use super::executor::TestStatus;
        if r.status != TestStatus::Passed {
            let reason = r.error_message.as_deref().unwrap_or("test failed");
            super::notify::notify_failure(&r.test_id, reason, r.duration_ms);

            // Write failure pattern to KB so future sessions can search by test_id.
            // Uses the existing tokio runtime — no-op if KB_ENABLED is not set.
            let test_id   = r.test_id.clone();
            let error_msg = r.error_message.clone().unwrap_or_default();
            let step_count = r.iterations;
            let duration   = r.duration_ms;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let topic_key = format!("sirin-failure-{test_id}");
                    let title     = format!("[FAIL] {test_id}");
                    let content   = format!(
                        "step: {step_count}\nerror: {error_msg}\nduration_ms: {duration}"
                    );
                    let _ = crate::kb_client::write_raw_to_project(
                        "sirin", &topic_key, &title, &content,
                        "testing", "test-failure", "",
                    ).await;
                });
            }
        } else {
            // Issue #33: capture successful selector/flow patterns on PASS.
            // Same fire-and-forget shape as the failure path; KB upserts on
            // identical topicKey so repeated CI passes auto-version rather
            // than spamming new entries.
            let test_id    = r.test_id.clone();
            let step_count = r.iterations;
            let duration   = r.duration_ms;
            let summary    = summarise_success_actions(&r.history);
            let analysis   = r.final_analysis.clone().unwrap_or_default();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let topic_key = format!("sirin-pass-{test_id}");
                    let title     = format!("[PASS] {test_id}");
                    let content   = format!(
                        "step: {step_count}\nduration_ms: {duration}\n\
                         actions:\n{summary}\n\
                         analysis: {analysis}"
                    );
                    let _ = crate::kb_client::write_raw_to_project(
                        "sirin", &topic_key, &title, &content,
                        "testing", "test-pass,selector-pattern", "",
                    ).await;
                });
            }
        }
    }
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        s.phase = phase;
        s.last_phase_updated_at = std::time::Instant::now(); // #182 idle watchdog
    }
}

/// Force-terminate a zombie run by setting it to Error state.
///
/// When a spawned test run gets stuck (e.g. LLM call hanging for >timeout_secs),
/// it stays in `Running` in-memory indefinitely because the executor deadline
/// check only fires between LLM calls.  Restarting Sirin is the only other option.
///
/// This function immediately sets the run to `Error("killed by user")` so:
/// - `get_test_result` returns the error state
/// - The next `run_test_async` caller gets the TEST_RUN_LOCK on next iteration
///   (the lock is held by the spawned OS thread; killing the phase here doesn't
///   release the lock immediately, but the next test will time-out and release it).
///
/// Returns Err if the run_id is not found.
pub fn kill_run(run_id: &str) -> Result<(), String> {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    match reg.get_mut(run_id) {
        Some(s) => {
            // Only kill if currently Running or Queued — don't overwrite completed runs.
            match s.phase {
                RunPhase::Running { .. } | RunPhase::Queued => {
                    s.phase = RunPhase::Error("killed by user".into());
                    Ok(())
                }
                _ => Err(format!("run '{run_id}' is already completed (phase is not Running/Queued)")),
            }
        }
        None => Err(format!("run '{run_id}' not found in in-memory registry")),
    }
}

/// Render the action sequence of a passing run as a compact bullet list so
/// future searches over `kbSearch("...selector...")` can recover the working
/// click target / form field path without dragging in noisy LLM thoughts.
///
/// Skips steps whose action JSON has no `action` key (defensive — shouldn't
/// happen) and truncates each `target` to 120 chars to keep payloads small.
/// At most 12 actions are emitted; a "…N more" tail line indicates trimming.
fn summarise_success_actions(history: &[super::executor::TestStep]) -> String {
    const MAX_LINES: usize = 12;
    const TARGET_TRUNC: usize = 120;
    let mut out = String::new();
    let total = history.len();
    for (i, step) in history.iter().take(MAX_LINES).enumerate() {
        let action = step.action.get("action").and_then(|v| v.as_str()).unwrap_or("?");
        // Pull the most useful descriptor — try common arg keys in order.
        let target = ["target", "selector", "url", "name", "role", "text"]
            .iter()
            .find_map(|k| step.action.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("");
        let target_short = if target.chars().count() > TARGET_TRUNC {
            let mut s: String = target.chars().take(TARGET_TRUNC).collect();
            s.push('…');
            s
        } else {
            target.to_string()
        };
        if target_short.is_empty() {
            out.push_str(&format!("- step{}: {}\n", i + 1, action));
        } else {
            out.push_str(&format!("- step{}: {} → {}\n", i + 1, action, target_short));
        }
    }
    if total > MAX_LINES {
        out.push_str(&format!("- …{} more\n", total - MAX_LINES));
    }
    if out.is_empty() {
        out.push_str("(no recorded actions)\n");
    }
    out
}

/// Push a full observation.  Step index is implicit (Vec length).
pub fn push_observation(run_id: &str, full_text: String) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        s.full_observations.push(full_text);
    }
}

/// Store screenshot bytes + optional error for later retrieval.
pub fn set_screenshot(run_id: &str, result: Result<Vec<u8>, String>) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        match result {
            Ok(bytes) => { s.screenshot_bytes = Some(bytes); s.screenshot_error = None; }
            Err(e) => { s.screenshot_bytes = None; s.screenshot_error = Some(e); }
        }
    }
}

pub fn get(run_id: &str) -> Option<RunState> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
        .get(run_id).cloned()
}

pub fn get_full_observation(run_id: &str, step: usize) -> Option<String> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
        .get(run_id)?.full_observations.get(step).cloned()
}

pub fn get_screenshot(run_id: &str) -> Option<(Option<Vec<u8>>, Option<String>)> {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let s = reg.get(run_id)?;
    Some((s.screenshot_bytes.clone(), s.screenshot_error.clone()))
}

/// Check screenshot cache for a vision analysis result.
/// `png_hash` should be SHA256(png_bytes) as hex string.
/// Returns cached analysis if found, None if cache miss.
pub fn get_screenshot_cache(run_id: &str, png_hash: &str) -> Option<String> {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.get(run_id)?.screenshot_cache.get(png_hash).cloned()
}

/// Store a vision analysis result in the screenshot cache.
/// `png_hash` should be SHA256(png_bytes) as hex string.
pub fn set_screenshot_cache(run_id: &str, png_hash: String, analysis: String) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        s.screenshot_cache.insert(png_hash, analysis);
    }
}

/// Get the A11y diff context for a test run.
/// Returns a clone of the context (safe for non-blocking reads).
pub fn get_ax_diff_context(run_id: &str) -> Option<crate::test_runner::ax_diff_context::AxDiffContext> {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.get(run_id).map(|s| s.ax_diff_context.clone())
}

/// Mutate the A11y diff context for a test run.
/// `f` is a closure that receives a mutable reference to the context.
pub fn mutate_ax_diff_context<F>(run_id: &str, f: F)
where
    F: FnOnce(&mut crate::test_runner::ax_diff_context::AxDiffContext),
{
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        f(&mut s.ax_diff_context);
    }
}

/// Store the most recent AX tree nodes for a test run (for SoM preparation).
/// Called by ax_tree tool handler to cache nodes for later SoM rendering.
pub fn set_recent_ax_nodes(run_id: &str, nodes: Vec<serde_json::Value>) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        s.recent_ax_nodes = Some(nodes);
    }
}

/// Retrieve the most recent AX tree nodes for a test run (for SoM preparation).
pub fn get_recent_ax_nodes(run_id: &str) -> Option<Vec<serde_json::Value>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
        .get(run_id)?.recent_ax_nodes.clone()
}

/// Store the SoM (Set-of-Mark) label map for a test run.
/// `label_map` maps label_id (1, 2, 3, ...) to (x, y) pixel coordinates.
pub fn set_som_label_map(run_id: &str, label_map: crate::test_runner::som_renderer::SoMLabelMap) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        s.som_label_map = Some(label_map);
    }
}

/// Retrieve the SoM label map for a test run.
pub fn get_som_label_map(run_id: &str) -> Option<crate::test_runner::som_renderer::SoMLabelMap> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
        .get(run_id)?.som_label_map.clone()
}


pub fn list_active() -> Vec<String> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(_, s)| matches!(s.phase, RunPhase::Running{..} | RunPhase::Queued))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Prune completed runs older than 1 hour to prevent unbounded growth.
fn prune_old_runs() {
    let cutoff = chrono::Local::now() - chrono::Duration::hours(1);
    let cutoff_str = cutoff.to_rfc3339();
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let stale: Vec<String> = reg.iter()
        .filter(|(_, s)| s.started_at.as_str() < cutoff_str.as_str()
            && matches!(s.phase, RunPhase::Complete(_) | RunPhase::Error(_)))
        .map(|(k, _)| k.clone())
        .collect();
    for key in stale {
        reg.remove(&key);
    }
}

// ── Serialization helper ─────────────────────────────────────────────────────

/// Serialize RunState to JSON for MCP response.
pub fn to_json(s: &RunState) -> serde_json::Value {
    use serde_json::json;
    let (status, extra) = match &s.phase {
        RunPhase::Queued => ("queued", json!({})),
        RunPhase::Running { step, current_action } => (
            "running",
            json!({ "step": step, "current_action": current_action }),
        ),
        RunPhase::Complete(r) => (
            match r.status {
                TestStatus::Passed   => "passed",
                TestStatus::Failed   => "failed",
                TestStatus::Timeout  => "timeout",
                TestStatus::Error    => "error",
                TestStatus::Disputed => "disputed",
            },
            json!({
                "iterations": r.iterations,
                "duration_ms": r.duration_ms,
                "error": r.error_message,
                "analysis": r.final_analysis,
                "steps": r.history.len(),
                "has_screenshot": s.screenshot_bytes.is_some(),
                "screenshot_error": s.screenshot_error,
            }),
        ),
        RunPhase::Error(e) => ("error", json!({ "error": e })),
    };
    // #182 — expose idle_secs for running tests so callers can detect stuck runs.
    // Only meaningful for Running phase; for terminal/queued phases it's 0.
    let idle_secs = match &s.phase {
        RunPhase::Running { .. } => s.last_phase_updated_at.elapsed().as_secs(),
        _ => 0,
    };
    json!({
        "run_id": s.run_id,
        "test_id": s.test_id,
        "started_at": s.started_at,
        "status": status,
        "idle_secs": idle_secs,
        "details": extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_run_creates_queued() {
        let id = new_run("test_x");
        let s = get(&id).unwrap();
        assert_eq!(s.test_id, "test_x");
        assert!(matches!(s.phase, RunPhase::Queued));
    }

    #[test]
    fn set_phase_updates() {
        let id = new_run("t");
        set_phase(&id, RunPhase::Running { step: 2, current_action: "click".into() });
        let s = get(&id).unwrap();
        if let RunPhase::Running { step, current_action } = s.phase {
            assert_eq!(step, 2);
            assert_eq!(current_action, "click");
        } else { panic!("expected running"); }
    }

    #[test]
    fn observations_accumulate() {
        let id = new_run("t");
        push_observation(&id, "first".into());
        push_observation(&id, "second".into());
        assert_eq!(get_full_observation(&id, 0).as_deref(), Some("first"));
        assert_eq!(get_full_observation(&id, 1).as_deref(), Some("second"));
        assert_eq!(get_full_observation(&id, 2), None);
    }

    #[test]
    fn screenshot_stored() {
        let id = new_run("t");
        set_screenshot(&id, Ok(vec![1, 2, 3]));
        let (bytes, err) = get_screenshot(&id).unwrap();
        assert_eq!(bytes, Some(vec![1, 2, 3]));
        assert_eq!(err, None);
    }

    #[test]
    fn screenshot_error_stored() {
        let id = new_run("t");
        set_screenshot(&id, Err("headless blank".into()));
        let (bytes, err) = get_screenshot(&id).unwrap();
        assert_eq!(bytes, None);
        assert_eq!(err.as_deref(), Some("headless blank"));
    }

    /// Cache stores by whatever string the caller passes as `png_hash` —
    /// builtins.rs uses a composite `<png>:<prompt>` key so different
    /// questions on the same screenshot don't collide.  This test mirrors
    /// that pattern and verifies independent get/set.
    #[test]
    fn screenshot_cache_separates_composite_keys() {
        let id = new_run("t");
        let png = "abcd1234"; // arbitrary png hash
        let key_q1 = format!("{png}:promptHash1______");
        let key_q2 = format!("{png}:promptHash2______");

        set_screenshot_cache(&id, key_q1.clone(), "answer for Q1".into());
        set_screenshot_cache(&id, key_q2.clone(), "answer for Q2".into());

        // Different question on same PNG must NOT return Q1's answer.
        assert_eq!(
            get_screenshot_cache(&id, &key_q2).as_deref(),
            Some("answer for Q2"),
        );
        assert_eq!(
            get_screenshot_cache(&id, &key_q1).as_deref(),
            Some("answer for Q1"),
        );
        // Unrelated key — miss.
        let key_other = format!("{png}:promptHash3______");
        assert!(get_screenshot_cache(&id, &key_other).is_none());
    }

    /// Same composite key returns the previously-cached answer (the cache
    /// IS supposed to dedupe identical question-on-same-PNG calls).
    #[test]
    fn screenshot_cache_same_key_is_cache_hit() {
        let id = new_run("t");
        let key = "deadbeef:promptHashAaa";
        set_screenshot_cache(&id, key.into(), "first call".into());
        assert_eq!(
            get_screenshot_cache(&id, key).as_deref(),
            Some("first call")
        );
    }

    /// Cache lookup on an unknown run_id returns None rather than panicking.
    #[test]
    fn screenshot_cache_missing_run_id_returns_none() {
        assert!(get_screenshot_cache("nonexistent_run", "any_key").is_none());
    }

    // ── Issue #33: success-pattern KB summarisation ──────────────────────

    use super::super::executor::TestStep;
    use serde_json::json;

    #[test]
    fn summarise_success_renders_action_target() {
        let history = vec![
            TestStep {
                thought: "go".into(),
                action: json!({"action":"goto","url":"https://x.test/"}),
                observation: "ok".into(),
                ..Default::default()
            },
            TestStep {
                thought: "click".into(),
                action: json!({"action":"click_text","text":"Continue"}),
                observation: "ok".into(),
                ..Default::default()
            },
        ];
        let out = summarise_success_actions(&history);
        assert!(out.contains("step1: goto → https://x.test/"), "got: {out}");
        assert!(out.contains("step2: click_text → Continue"),  "got: {out}");
    }

    #[test]
    fn summarise_success_truncates_long_target() {
        let long = "a".repeat(500);
        let history = vec![TestStep {
            thought: String::new(),
            action: json!({"action":"click","selector":long}),
            observation: String::new(),
            ..Default::default()
        }];
        let out = summarise_success_actions(&history);
        // 120 chars + ellipsis
        assert!(out.contains('…'), "expected ellipsis, got: {out}");
        assert!(out.len() < 200, "expected truncation, got len={}", out.len());
    }

    #[test]
    fn summarise_success_caps_at_max_lines_and_emits_tail() {
        let mk = |i: usize| TestStep {
            thought: String::new(),
            action: json!({"action":"step","name":format!("n{i}")}),
            observation: String::new(),
            ..Default::default()
        };
        let history: Vec<TestStep> = (0..20).map(mk).collect();
        let out = summarise_success_actions(&history);
        // 12 lines + 1 tail = 13 lines
        let lines = out.lines().count();
        assert_eq!(lines, 13, "got: {out}");
        assert!(out.contains("…8 more"), "got: {out}");
    }

    #[test]
    fn summarise_success_handles_empty_history() {
        let out = summarise_success_actions(&[]);
        assert!(out.contains("no recorded actions"), "got: {out}");
    }

    #[test]
    fn summarise_success_falls_back_when_no_target_keys() {
        let history = vec![TestStep {
            thought: String::new(),
            action: json!({"action":"wait_for_idle"}),
            observation: String::new(),
            ..Default::default()
        }];
        let out = summarise_success_actions(&history);
        assert!(out.contains("step1: wait_for_idle"), "got: {out}");
        assert!(!out.contains(" → "), "should not render arrow w/o target: {out}");
    }
}
