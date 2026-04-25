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
pub fn set_phase(run_id: &str, phase: RunPhase) {
    // Trigger failure notification before storing (fire-and-forget).
    if let RunPhase::Complete(ref r) = phase {
        use super::executor::TestStatus;
        if r.status != TestStatus::Passed {
            let reason = r.error_message.as_deref().unwrap_or("test failed");
            super::notify::notify_failure(&r.test_id, reason, r.duration_ms);
        }
    }
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = reg.get_mut(run_id) {
        s.phase = phase;
    }
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
                TestStatus::Passed  => "passed",
                TestStatus::Failed  => "failed",
                TestStatus::Timeout => "timeout",
                TestStatus::Error   => "error",
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
    json!({
        "run_id": s.run_id,
        "test_id": s.test_id,
        "started_at": s.started_at,
        "status": status,
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
}
