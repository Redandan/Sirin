//! ReAct-style test executor — LLM drives browser actions to achieve a goal.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::parser::TestGoal;

// ── docs_refs cache ──────────────────────────────────────────────────────────
//
// Resolved docs_refs content is cached per-test-id so the prompt builders can
// splice it in synchronously even though resolution (filesystem + KB MCP) is
// async.  `pre_resolve_docs_for` populates the cache once at run start;
// `cached_docs` reads it from the four `build_prompt*` variants below.
//
// Bounded growth: bounded by number of distinct test_ids per process lifetime,
// which is small (<200).  No eviction needed.

fn docs_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_docs(test_id: &str) -> Option<String> {
    docs_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(test_id)
        .cloned()
}

// ── KB-hits cache (Issue #39 trace) ──────────────────────────────────────────
//
// Records the KB topicKeys that successfully resolved for each test_id during
// `pre_resolve_docs_for`.  Lets each TestStep carry a `kb_hits` snapshot so
// `get_run_trace` can show "this run injected lessons X, Y, Z" without
// re-parsing the prompt.

fn kb_hits_cache() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_kb_hit(test_id: &str, key: &str) {
    let mut g = kb_hits_cache().lock().unwrap_or_else(|e| e.into_inner());
    let entry = g.entry(test_id.to_string()).or_default();
    if !entry.iter().any(|k| k == key) {
        entry.push(key.to_string());
    }
}

fn cached_kb_hits(test_id: &str) -> Vec<String> {
    kb_hits_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(test_id)
        .cloned()
        .unwrap_or_default()
}

/// Resolve `test.docs_refs` + `test.kb_refs` into a single Markdown block
/// ready to splice into the LLM prompt.
///
/// `docs_refs` (mixed): each entry is auto-classified —
/// - **Filesystem path** (contains `/`/`\` or has 1-5 char alpha extension) →
///   read with `std::fs::read_to_string`, truncated to ~2000 chars
/// - **KB topicKey** (kebab-case, no path/extension) → fetch via
///   [`crate::kb_client::get`]
///
/// `kb_refs` (KB-only): every entry is fetched as a topicKey unconditionally,
/// no path heuristic.  Use this when intent is explicitly KB-only and you
/// want to bypass the docs_refs path-vs-key auto-detection edge cases.
///
/// Stores the result in [`docs_cache`] keyed by `test.id`; subsequent
/// `cached_docs(test.id)` calls inside the prompt builders return it
/// synchronously.  Safe to call multiple times — overwrites previous cache.
///
/// Failures are silently demoted to `[unavailable: <reason>]` lines so a
/// missing file or KB outage never aborts a test run.
pub(crate) async fn pre_resolve_docs_for(test: &TestGoal) {
    if test.docs_refs.is_empty() && test.kb_refs.is_empty() {
        return;
    }
    let project = crate::kb_client::default_project();
    let mut sections: Vec<String> = Vec::with_capacity(test.docs_refs.len() + test.kb_refs.len());

    // 1) docs_refs (mixed: path or KB topicKey).  Filesystem misses are
    // still rendered as "[unavailable]" so authors see the typo; KB misses
    // when KB is disabled are silently SKIPPED so disabled-KB doesn't spam
    // the prompt with unhelpful "[unavailable: KB disabled]" lines.
    for r in &test.docs_refs {
        let entry = r.trim();
        if entry.is_empty() {
            continue;
        }
        if crate::kb_client::looks_like_topic_key(entry) {
            match crate::kb_client::get(&project, entry).await {
                Ok(Some(text)) => {
                    record_kb_hit(&test.id, entry);
                    sections.push(format!(
                        "### KB:{project}/{entry}\n{}", truncate(&text, 2000)
                    ));
                }
                Ok(None) => {
                    // KB disabled or entry not found — skip silently.
                    if crate::kb_client::enabled() {
                        tracing::debug!(
                            "[test_runner] docs_refs '{entry}' resolved to no KB entry — skipping"
                        );
                    }
                }
                Err(e) => sections.push(format!(
                    "### KB:{project}/{entry}\n[unavailable: {e}]"
                )),
            }
        } else {
            // Filesystem path — render unavailable inline so authors see typos.
            let path = std::path::Path::new(entry);
            match std::fs::read_to_string(path) {
                Ok(text) => sections.push(format!(
                    "### {entry}\n{}", truncate(&text, 2000)
                )),
                Err(e) => sections.push(format!(
                    "### {entry}\n[unavailable: {e}]"
                )),
            }
        };
    }

    // 2) kb_refs (KB-only).  Silent skip on disabled-KB / missing entry —
    // the explicit field signals "I want KB content" so absence shouldn't
    // waste prompt tokens on placeholders.
    for r in &test.kb_refs {
        let entry = r.trim();
        if entry.is_empty() {
            continue;
        }
        match crate::kb_client::get(&project, entry).await {
            Ok(Some(text)) => {
                record_kb_hit(&test.id, entry);
                sections.push(format!(
                    "### KB:{project}/{entry}\n{}", truncate(&text, 2000)
                ));
            }
            Ok(None) => {
                if crate::kb_client::enabled() {
                    tracing::debug!(
                        "[test_runner] kb_refs '{entry}' missing in KB — skipping"
                    );
                }
            }
            Err(e) => sections.push(format!(
                "### KB:{project}/{entry}\n[unavailable: {e}]"
            )),
        }
    }

    let block = sections.join("\n\n");
    docs_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(test.id.clone(), block);
}

/// Format the cached docs block for embedding in a prompt.  Returns empty
/// string when no docs were resolved (so the prompt template stays clean).
fn docs_prompt_block(test_id: &str) -> String {
    match cached_docs(test_id) {
        Some(s) if !s.is_empty() => format!(
            "\n## Required reading (from docs_refs — read BEFORE acting)\n{s}\n"
        ),
        _ => String::new(),
    }
}

/// Sanitise an arbitrary string into a topicKey-safe slug.
///
/// Strips non-alphanumeric (keeps `-` `_`), lower-cases, dedups consecutive
/// separators, and trims to `max_len`.  Used to turn action signatures like
/// `shadow_click:tab:^商品$` into deterministic kb topic keys.
fn slugify_for_topic(s: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_sep = false;
        } else if matches!(ch, '-' | '_') {
            if !prev_sep {
                out.push('-');
                prev_sep = true;
            }
        } else if !prev_sep {
            out.push('-');
            prev_sep = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(max_len).collect()
}

/// Best-effort: write a KB raw note when a convergence/error-ratio guard
/// fires, capturing the test_id + action signature + observation snippet so
/// future debug sessions can `kbSearch` for "stuck-loop" patterns instead of
/// rediscovering them by hand.
///
/// Spawned as a fire-and-forget task — the test abort flow MUST NOT wait on
/// the KB MCP round-trip.  Failures (KB disabled, MCP down, etc.) are
/// silently swallowed by `kb_client::write_raw`.
fn record_guard_fire_to_kb(
    test_id: String,
    iteration: u32,
    guard_kind: &'static str,
    signature: String,
    observation_snippet: String,
) {
    let title = format!("Stuck loop: {test_id} ({guard_kind})");
    let topic_key = format!(
        "stuck-{}-{}",
        slugify_for_topic(&test_id, 40),
        slugify_for_topic(&signature, 40),
    );
    let content = format!(
        "## What happened\n\
         Test `{test_id}` aborted at iteration {iteration} via the **{guard_kind}** guard.\n\n\
         ## Action signature that repeated\n\
         `{signature}`\n\n\
         ## Last observation snippet\n\
         ```\n{}\n```\n\n\
         ## Why this matters\n\
         Repeated occurrences of this stuck pattern indicate either:\n\
         - The target element is genuinely missing on the current page state\n\
         - The YAML step is wrong about the role/name regex\n\
         - The Flutter semantics tree didn't bootstrap before the action fired\n\
         Search KB for `stuck-{}` to see related patterns across runs.",
        truncate(&observation_snippet, 500),
        slugify_for_topic(&test_id, 40),
    );
    let file_refs = format!("config/tests/{test_id}.yaml");
    let tags = format!("stuck-loop,{guard_kind},guard,test-flake");
    tokio::spawn(async move {
        let _ = crate::kb_client::write_raw(
            &topic_key,
            &title,
            &content,
            "testing",
            &tags,
            &file_refs,
        ).await;
    });
}

/// Handle the `dispute_yaml` action emitted by the LLM.
///
/// Three side-effects (all fire-and-forget):
/// 1. KB write — `yaml-dispute-{test_id}-{run_id}` draft note (if KB_ENABLED=1).
/// 2. `gh issue create` — opens a GitHub issue on Redandan/Sirin with structured body.
///    Uses `std::process::Command` so no new crate dependency is needed.
///
/// Returns the `DisputeInfo` for the caller to embed in the `TestResult`.
fn handle_dispute_yaml(
    test_id: &str,
    run_id: Option<&str>,
    iteration: u32,
    dispute: DisputeInfo,
    final_url: &str,
    llm_model: &str,
) -> DisputeInfo {
    let ts = chrono::Local::now().to_rfc3339();
    let rid = run_id.unwrap_or("unknown");

    // ── Layer 2: auto kbWrite ────────────────────────────────────────────────
    {
        let topic_key = format!(
            "yaml-dispute-{}-{}",
            slugify_for_topic(test_id, 40),
            slugify_for_topic(rid, 20),
        );
        let title = format!("yaml-dispute({test_id}): iter {iteration} — {}", truncate(&dispute.reason, 60));
        let step_note = dispute
            .suspected_step
            .map(|n| format!("Suspected step: {n}"))
            .unwrap_or_else(|| "Suspected step: (unspecified)".into());
        let fix_note = dispute
            .suggested_fix
            .as_deref()
            .map(|f| format!("Suggested fix: {f}"))
            .unwrap_or_else(|| "Suggested fix: (none)".into());
        let content = format!(
            "## Dispute report\n\
             **Test**: `{test_id}`  \n\
             **Run ID**: `{rid}`  \n\
             **LLM**: {llm_model}  \n\
             **Date**: {ts}  \n\
             **Iteration when dispute fired**: {iteration}\n\n\
             ## Reason (LLM-provided)\n\
             > {reason}\n\n\
             ## {step_note}\n\n\
             ## {fix_note}\n\n\
             ## Context\n\
             - Final URL: {final_url}\n\
             - KB entry: `{topic_key}`\n",
            reason = dispute.reason,
        );
        let tags = format!("yaml-dispute,test-id-{}", slugify_for_topic(test_id, 30));
        let tid = test_id.to_string();
        tokio::spawn(async move {
            let _ = crate::kb_client::write_raw_to_project(
                &crate::kb_client::default_project(),
                &topic_key,
                &title,
                &content,
                "test-yaml-dispute",
                &tags,
                &format!("config/tests/{tid}.yaml"),
            ).await;
        });
    }

    // ── Layer 3: auto gh issue create ────────────────────────────────────────
    {
        let step_n = dispute.suspected_step.unwrap_or(-1);
        let reason_short: String = dispute.reason.chars().take(50).collect();
        let gh_title = if step_n >= 0 {
            format!("yaml-dispute({test_id}): step {step_n} — {reason_short}")
        } else {
            format!("yaml-dispute({test_id}): {reason_short}")
        };
        let fix_section = dispute
            .suggested_fix
            .as_deref()
            .map(|f| format!("> {f}"))
            .unwrap_or_else(|| "(none provided)".into());
        let step_section = if step_n >= 0 {
            format!("step {step_n}: *(see YAML)*")
        } else {
            "(unspecified)".into()
        };
        let slug_test = slugify_for_topic(test_id, 40);
        let slug_rid  = slugify_for_topic(rid, 20);
        let gh_body = format!(
            "## LLM Dispute Report\n\n\
             **Test**: {test_id}\n\
             **Run ID**: {rid}\n\
             **LLM**: {llm_model}\n\
             **Date**: {ts}\n\n\
             ### Reason (LLM-provided)\n\
             > {reason}\n\n\
             ### Suspected step\n\
             {step_section}\n\n\
             ### Suggested fix (LLM-provided, 僅供參考)\n\
             {fix_section}\n\n\
             ### Context\n\
             - URL: {final_url}\n\
             - Iterations before dispute: {iteration}\n\n\
             ### Raw context\n\
             - KB entry: `yaml-dispute-{slug_test}-{slug_rid}`\n",
            reason = dispute.reason,
        );
        let labels = "yaml-dispute,bot-flagged,triage";
        match std::process::Command::new("gh")
            .args([
                "issue", "create",
                "--repo", "Redandan/Sirin",
                "--title", &gh_title,
                "--body", &gh_body,
                "--label", labels,
            ])
            .output()
        {
            Ok(out) if out.status.success() => {
                let url = String::from_utf8_lossy(&out.stdout);
                tracing::info!(
                    "[dispute_yaml] '{}' — GitHub issue created: {}",
                    test_id,
                    url.trim()
                );
            }
            Ok(out) => {
                tracing::warn!(
                    "[dispute_yaml] '{}' — gh issue create failed (exit {}): {}",
                    test_id,
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[dispute_yaml] '{}' — gh CLI not available or spawn failed: {e}",
                    test_id
                );
            }
        }
    }

    dispute
}

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestStep {
    pub thought: String,
    pub action: Value,        // {"action":"click","target":"#btn"}
    pub observation: String,  // truncated tool result or ERROR:...
    // ── Per-step trace metadata (Issue #39) ──────────────────────────────────
    // All fields are Option / default-empty so old `history_json` blobs in
    // SQLite deserialize cleanly when these columns are absent.
    /// Resolved LLM model that produced this step (e.g. `gemini-2.0-flash`,
    /// `claude_cli`).  None for steps recorded before recent_iterations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    /// Wall-clock duration of the LLM call that emitted this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_latency_ms: Option<u64>,
    /// Token count if the backend returned usage metadata (currently none do
    /// — placeholder so the schema is forward-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_tokens: Option<u32>,
    /// KB topicKeys that were injected into the prompt for this run (all
    /// steps share the same set — duplicated for trace simplicity).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kb_hits: Vec<String>,
    /// Number of JSON parse retries that preceded this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_errors: Option<u32>,
    /// ISO 8601 timestamp when the step was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub test_id: String,
    pub status: TestStatus,
    pub iterations: u32,
    pub duration_ms: u64,
    pub error_message: Option<String>,
    pub screenshot_path: Option<String>,
    #[serde(default)]
    pub screenshot_error: Option<String>,
    pub history: Vec<TestStep>,
    pub final_analysis: Option<String>,
    /// Populated when `status == Disputed` — carries the LLM-supplied
    /// dispute payload from the `dispute_yaml` action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispute: Option<DisputeInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Passed,
    Failed,
    Timeout,
    Error,
    /// LLM identified a YAML/spec issue and called `dispute_yaml`.
    /// Not a test failure — a signal that the spec needs human review.
    /// Sirin auto-opens a GitHub issue and writes a KB entry.
    Disputed,
}

/// Dispute metadata written when `dispute_yaml` action fires.
/// All fields are LLM-provided and informational only — never auto-acted on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisputeInfo {
    pub reason: String,
    pub suspected_step: Option<i64>,
    pub suggested_fix: Option<String>,
}

// ── Executor ─────────────────────────────────────────────────────────────────

/// Truncate observation text past this many chars in LLM history.
/// Default: 800 chars for normal tests.
/// Vision-heavy tests (with frequent screenshots) use more aggressive 500 chars to save tokens.
const OBS_TRUNCATE_CHARS: usize = 800;
const OBS_TRUNCATE_CHARS_VISION_HEAVY: usize = 500;

/// Merge a `session_id` field into a browser action's JSON args (no-op if
/// the caller didn't request a session).  Used to fan a single test out
/// onto a dedicated chrome tab when `run_test_batch` runs N tests in
/// parallel.  Browser actions that don't recognise the field ignore it.
/// Returns `true` if the screenshot looks like a completely black/blank frame.
///
/// Uses two heuristics:
/// 1. `size_bytes` < 8 000 — all-black PNGs compress to near-nothing (~2 KB).
///    A real rendered page (Flutter, React, etc.) is always ≥ 15 KB.
/// 2. `url` is `about:blank` — browser hasn't navigated yet (shouldn't happen
///    after a successful `goto`, but guards against race conditions).
///
/// This catches the Flutter CanvasKit "headless = blank canvas" failure without
/// needing a base64 decoder dependency.
fn is_all_black_screenshot(ss_val: &Value) -> bool {
    // Guard: if the result has an error key, don't treat it as black
    if ss_val.get("error").is_some() {
        return false;
    }

    let size_bytes = ss_val.get("size_bytes").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
    let url = ss_val.get("url").and_then(|v| v.as_str()).unwrap_or("");

    if url == "about:blank" {
        return true;
    }

    // Real rendered pages (Flutter HTML renderer, SPA): typically ≥ 15 000 bytes.
    // Truly all-black / about:blank: ≤ 3 000 bytes.
    // Near-black (Chrome crashed during Flutter init, recovery just launched):
    //   observed at ~12 000 bytes — just above the old 8 000 threshold.
    // Threshold 14 000 catches all known rendering-failure cases while staying
    // well below the ≥ 15 000 floor of real rendered pages.
    size_bytes < 14_000
}

fn inject_session(args: &mut Value, session_id: Option<&str>) {
    if let (Some(sid), Some(obj)) = (session_id, args.as_object_mut()) {
        // Don't overwrite if the LLM (or fixture) explicitly set its own.
        obj.entry("session_id").or_insert_with(|| json!(sid));
    }
}

/// Extract the scheme+host origin from a URL.
/// e.g. `https://redandan.github.io/#/login` → `https://redandan.github.io`
fn extract_origin(url: &str) -> String {
    if let Some(after_scheme) = url.find("://").map(|p| &url[p + 3..]) {
        let host_end = after_scheme.find(['/', '?', '#']).unwrap_or(after_scheme.len());
        let scheme   = &url[..url.find("://").unwrap()];
        format!("{scheme}://{}", &after_scheme[..host_end])
    } else {
        url.to_string()
    }
}

/// Run a single fixture step via the `web_navigate` tool.
async fn run_fixture_step(
    ctx: &crate::adk::context::AgentContext,
    step: &crate::test_runner::parser::FixtureStep,
    session_id: Option<&str>,
) -> Result<(), String> {
    let mut args = json!({
        "action": step.action,
        "target": step.target,
        "text": step.text,
    });
    if let Some(ms) = step.timeout_ms {
        args["timeout"] = json!(ms);
    }
    // Forward any extra browser_exec params (e.g. role, name_regex for shadow_click).
    for (k, v) in &step.extra {
        args[k] = v.clone();
    }
    inject_session(&mut args, session_id);
    ctx.call_tool("web_navigate", args).await
        .map(|_| ())
        .map_err(|e| format!("fixture step '{}' failed: {}", step.action, e))
}

/// Execute a test goal by driving the browser via the `web_navigate` tool.
pub async fn execute_test(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
) -> TestResult {
    execute_test_tracked(ctx, test, None, None).await
}

/// Same as [`execute_test`] but reports live progress to an async run registry.
/// `run_id` — key in [`crate::test_runner::runs`] to update as steps progress.
/// `session_id` — when `Some`, every browser tool call gets a `session_id` field
/// merged into its args, isolating this run to a dedicated chrome tab.  Used by
/// `run_test_batch` to fan out parallel runs over independent tabs.
pub async fn execute_test_tracked(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
    run_id: Option<&str>,
    session_id: Option<&str>,
) -> TestResult {
    use crate::test_runner::runs;

    let started = std::time::Instant::now();
    let mut history: Vec<TestStep> = Vec::new();
    let mut parse_error_hint: Option<String> = None;
    let mut parse_error_count = 0u32;
    let max_parse_errors = test.retry_on_parse_error.max(1);

    // Issue #78: per-iteration screenshot ring buffer; encoded into a
    // timeline.gif iff the test fails AND `record_timeline_gif` is on.
    let mut timeline = crate::test_runner::gif_recorder::TimelineBuffer::new();
    let record_timeline = test.record_timeline_gif;

    if let Some(rid) = run_id {
        runs::set_phase(rid, runs::RunPhase::Running { step: 0, current_action: "goto".into() });
    }

    // Vision specialist startup log (once per test, not per iteration).
    match crate::llm::vision_llm_config() {
        Some(ref v) => tracing::info!(
            target: "sirin",
            "[llm] vision specialist: backend={:?} model={}",
            v.backend, v.model
        ),
        None => tracing::info!(
            target: "sirin",
            "[llm] vision specialist: (none, using main model)"
        ),
    }

    // 0-pre) Resolve docs_refs into a Markdown block the prompt builders can
    // splice in.  Each entry is read from disk (filesystem path) or fetched
    // from the central KB (topicKey).  Cached per test_id for the duration
    // of the run so the four prompt variants get it for free.  Failures are
    // demoted to "[unavailable: …]" — never aborts the test.
    pre_resolve_docs_for(test).await;

    if !test.docs_refs.is_empty() {
        tracing::warn!(
            "[test_runner] ⚠️  '{}' has {} required doc(s) — auto-injected into prompt:\n{}",
            test.id,
            test.docs_refs.len(),
            test.docs_refs.iter().map(|d| format!("  • {d}")).collect::<Vec<_>>().join("\n")
        );
    }

    // 0-priv) Privacy mask (Issue #80): default ON — fail-secure.  Restored to
    // the previous global value when this run finishes (so a `mask_sensitive:
    // false` test does not leak its preference into the next test).
    let want_mask = test.mask_sensitive.unwrap_or(true);
    let prev_mask = crate::browser::set_privacy_mask(want_mask);
    /// RAII guard that restores the global privacy-mask flag on drop, even if
    /// the test panics or returns early.
    struct MaskGuard(bool);
    impl Drop for MaskGuard {
        fn drop(&mut self) {
            crate::browser::set_privacy_mask(self.0);
        }
    }
    let _mask_guard = MaskGuard(prev_mask);

    // 0-ind) Action indicator (Issue #75) — opt-in.  Toggle the global flag
    // and install a RAII guard that restores the previous value AND removes
    // the in-page DOM nodes on drop (so the next test starts clean even if
    // it doesn't opt in).
    let prev_indicator = crate::browser::set_action_indicator(test.show_action_indicator);
    /// RAII guard for the action indicator: restores the previous global
    /// flag and tears down the DOM badge/border on drop.
    struct IndicatorGuard(bool);
    impl Drop for IndicatorGuard {
        fn drop(&mut self) {
            crate::browser::set_action_indicator(self.0);
            // Best-effort DOM cleanup — runs even on panic / early return.
            // Failures are debug-logged inside `hide_action_indicator`.
            crate::browser::hide_action_indicator();
        }
    }
    let _indicator_guard = IndicatorGuard(prev_indicator);

    // 0) Ensure browser launched in the right headless mode.
    // Flutter CanvasKit/WebGL needs headless=false to actually paint.
    let want_headless = test.browser_headless.unwrap_or_else(crate::browser::default_headless);
    if let Err(e) = tokio::task::spawn_blocking(move || {
        // Register the desired mode BEFORE ensure_open so that mid-call
        // recovery in with_tab() can re-launch in the same mode, not
        // the process default (which is always headless=true).
        crate::browser::set_test_headless_mode(want_headless);
        crate::browser::ensure_open(want_headless)
    })
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))
        .and_then(|r| r)
    {
        return finalize_early(ctx, run_id, test, &history, format!("browser launch failed: {e}")).await;
    }

    // 1) Navigate to the test URL (with url_query params merged in).
    let nav_url = test.full_url();

    // 1-pre) Wipe origin storage BEFORE the first goto so Flutter sees an empty
    //        profile from frame zero.
    //
    //        Triggered for:
    //        a) fixture tests — fixture handles login via shadow_click; clearing
    //           here prevents cross-test session leakage.
    //        b) `?__test_role=` URL tests — Flutter auto-logs in from the URL
    //           param; we must start with a clean profile for the same reason.
    //
    //        Why here and not after load: clear_browser_state() runs JS on the
    //        already-loaded page, but Flutter has already read localStorage into
    //        memory by then — clearing storage does not un-authenticate the live
    //        app.  CDP Storage.clearDataForOrigin operates on Chrome's profile
    //        database and takes effect before the next navigation.
    //
    //        The tab is created here (session_switch) so CDP has a target to
    //        send the command to; it does NOT need to be on that origin yet.
    if test.fixture.is_some() || nav_url.contains("__test_role=") {
        let origin = extract_origin(&nav_url);
        let sid_pre = session_id.map(|s| s.to_string());
        let clear_result = tokio::task::spawn_blocking(move || {
            if let Some(s) = sid_pre.as_deref() {
                let _ = crate::browser::session_switch(s);
            }
            crate::browser::clear_origin_data(&origin)
        }).await;
        match clear_result {
            Ok(Ok(())) => tracing::debug!("[test_runner] '{}' — pre-navigate origin clear OK", test.id),
            Ok(Err(e)) => tracing::warn!("[test_runner] '{}' — pre-navigate origin clear failed (non-fatal): {e}", test.id),
            Err(e)     => tracing::warn!("[test_runner] '{}' — pre-navigate clear spawn error: {e}", test.id),
        }
    }

    let mut nav_input = json!({ "action": "goto", "target": &nav_url });
    inject_session(&mut nav_input, session_id);
    if let Err(e) = ctx.call_tool("web_navigate", nav_input).await {
        return finalize_early(ctx, run_id, test, &history, format!("navigate failed: {e}")).await;
    }

    // 1b) Install console + network capture IMMEDIATELY after navigate.
    //
    // CRITICAL ORDER: install_capture MUST come before the wait and
    // black-screen screenshot check.
    //
    // Why: headless_chrome drops the CDP WebSocket if no events arrive for
    // 30 s.  During Flutter's JS initialisation (SwiftShader WebGL + Dart
    // engine boot) Chrome can be silent for 30-40 s, causing the CDP
    // "timeout while listening for browser events" error (false crash).
    //
    // install_capture subscribes to Network.*, Console.*, and Page.* events.
    // As Flutter loads its Dart/JS bundle (many network requests) Chrome emits
    // events that reset the 30-s timer — keeping the connection alive while
    // Flutter boots silently from the JS perspective.
    {
        let mut cap_input = json!({ "action": "install_capture" });
        inject_session(&mut cap_input, session_id);
        let _ = ctx.call_tool("web_navigate", cap_input).await;
    }

    // 1c) Black-screen guard: wait 8 s for Flutter / SPA to initialise, then
    // take a screenshot and check if the page is all-black.
    // The wait gives Flutter enough time to render its first frame.
    // install_capture (above) keeps the CDP connection alive during this wait.
    //
    // For fixture tests we SKIP the built-in 8 s wait: the fixture's own
    // first step is `wait 8000` which covers Flutter boot.  Doing both wastes
    // 8 s per test (≈ 96 s on a 12-test suite) with no benefit.
    {
        if test.fixture.is_none() {
            let mut wait_input = json!({"action": "wait", "timeout_ms": 8000});
            inject_session(&mut wait_input, session_id);
            let _ = ctx.call_tool("web_navigate", wait_input).await;
        }

        let mut ss_input = json!({"action": "screenshot"});
        inject_session(&mut ss_input, session_id);
        if let Ok(ss_val) = ctx.call_tool("web_navigate", ss_input).await {
            if is_all_black_screenshot(&ss_val) {
                tracing::warn!(
                    "[test_runner] ⚠️  '{}' — post-navigate screenshot is all-black. \
                     Likely Chrome recovered in headless mode. Resetting browser and retrying navigate.",
                    test.id
                );
                // Force-close and re-open in the correct mode.
                let _ = tokio::task::spawn_blocking(move || {
                    crate::browser::close();
                    crate::browser::set_test_headless_mode(want_headless);
                    crate::browser::ensure_open(want_headless)
                }).await;
                // Re-subscribe to events on the new Chrome instance.
                let mut cap2 = json!({ "action": "install_capture" });
                inject_session(&mut cap2, session_id);
                let _ = ctx.call_tool("web_navigate", cap2).await;
                // Re-navigate.
                let mut nav2 = json!({ "action": "goto", "target": &nav_url });
                inject_session(&mut nav2, session_id);
                if let Err(e) = ctx.call_tool("web_navigate", nav2).await {
                    return finalize_early(ctx, run_id, test, &history,
                        format!("navigate retry after black-screen reset failed: {e}")).await;
                }
            }
        }
    }

    // 2c-pre) For URL-based auto-login (?__test_role=), call enable_a11y after the
    //         8 s Flutter-boot wait so the Flutter semantics tree is enabled on the
    //         *logged-in* home page.  This mirrors what the old fixture did with its
    //         second `enable_a11y` step (the one that ran after the shadow_click login).
    //         Without it the AX tree often collapses mid-test.
    if nav_url.contains("__test_role=") && test.fixture.is_none() {
        let mut ea = json!({"action": "enable_a11y"});
        inject_session(&mut ea, session_id);
        let _ = ctx.call_tool("web_navigate", ea).await;
        tracing::debug!("[test_runner] '{}' — post-auto-login enable_a11y OK", test.id);
    }

    // Issue #75: inject the action indicator now that the page is ready
    // (no-op if `show_action_indicator` is false).  We do this AFTER
    // navigate so it lands on the test target page, not the previous one.
    if test.show_action_indicator {
        let _ = tokio::task::spawn_blocking(|| {
            crate::browser::show_action_indicator("starting");
        }).await;
    }

    // 2c) Run fixture setup steps (failure aborts the test before the ReAct loop).
    if let Some(fixture) = &test.fixture {
        for step in &fixture.setup {
            if let Err(e) = run_fixture_step(ctx, step, session_id).await {
                let result = finalize_early(ctx, run_id, test, &history, format!("fixture setup failed: {e}")).await;
                // Still run cleanup even when setup fails.
                if let Some(fix) = &test.fixture {
                    for cs in &fix.cleanup {
                        if let Err(ce) = run_fixture_step(ctx, cs, session_id).await {
                            tracing::warn!("[fixture] cleanup step '{}' failed: {ce}", cs.action);
                        }
                    }
                }
                return result;
            }
        }
    }

    // 3) ReAct loop
    let max_iter = test.max_iterations.max(1);
    let deadline = started + std::time::Duration::from_secs(test.timeout_secs.max(10));
    // Convergence guard: track recent action signatures so we can break out of
    // genuine LLM stuck-loops (same action repeated despite same observation).
    // - Window LOOP_WINDOW captures the last N *non-noise* actions only — `wait`
    //   and a11y bootstrap calls are excluded so they don't dilute the signal
    //   when the LLM does "fail → wait → fail → wait" sequences (run_..._11
    //   service test hit exactly this).
    // - LOOP_THRESHOLD = repeat count that trips the guard.
    let mut recent_action_sigs: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(8);
    const LOOP_WINDOW: usize = 8;
    const LOOP_THRESHOLD: usize = 4;
    // Error-rate guard: track whether the last LOOP_WINDOW observations were
    // errors.  If ERROR_RATIO_NUM/ERROR_RATIO_DEN of recent observations are
    // errors, abort — the LLM is varying actions but nothing is working.
    let mut recent_obs_was_error: std::collections::VecDeque<bool> =
        std::collections::VecDeque::with_capacity(8);
    const ERROR_RATIO_NUM: usize = 6;  // 6 of last 8 = 75%

    // Collect the loop result into a variable so cleanup always runs afterward.
    let run_result: TestResult = 'react: {
        for iteration in 0..max_iter {
            if std::time::Instant::now() >= deadline {
                let cap = capture_screenshot(ctx, &test.id, run_id).await;
                break 'react TestResult {
                    test_id: test.id.clone(),
                    status: TestStatus::Timeout,
                    iterations: iteration,
                    duration_ms: started.elapsed().as_millis() as u64,
                    error_message: Some(format!("timed out after {}s", test.timeout_secs)),
                    screenshot_path: cap.path,
                    screenshot_error: cap.error,
                    history,
                    final_analysis: None,
                    dispute: None,
                };
            }

            // Reset parse-error hint each turn — but if we're past 70% of the
            // budget without a done=true, layer in a done-nudge so the LLM
            // doesn't burn the remaining budget on superfluous verification
            // turns.  Empirically this saves the "completed work but never
            // emitted done=true" pathology (run_20260425_215841_159_0 hit
            // max_iter at step 24 right after a successful screenshot_analyze).
            let hint_for_llm = match parse_error_hint.take() {
                Some(h) => Some(h),
                None if iteration > 0 && iteration * 10 >= max_iter * 7 => Some(format!(
                    "⚠️ You've used {iter}/{max} iterations.  If every \
                     success_criterion is satisfied by the observations above, \
                     output `{{\"thought\":\"...\",\"done\":true,\"final_answer\":\"<summary>\"}}` \
                     NOW.  Don't burn iterations on extra verification screenshots — \
                     trust what you've already seen.",
                    iter = iteration, max = max_iter
                )),
                None => None,
            };

            // Perception capture — zero-overhead for PerceptionMode::Text (short-
            // circuits inside perceive).  Done once per iteration and reused across
            // the 3 LLM retry attempts below so we don't re-screenshot on transient
            // backend errors.
            let perception = crate::perception::perceive(ctx, test.perception).await;

            // LLM call with retry for transient network errors (e.g. "error decoding
            // response body" from Gemini when Chrome crashes cause concurrent request
            // interference).  We retry up to 3× with a short back-off before giving up.
            let (raw, llm_meta) = {
                let mut last_err = String::new();
                let mut raw_opt: Option<(String, LlmCallMeta)> = None;
                for attempt in 0u32..3 {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    match call_test_llm(ctx, test, &history, hint_for_llm.as_deref(), &perception).await {
                        Ok(pair) => { raw_opt = Some(pair); break; }
                        Err(e) => {
                            tracing::warn!(
                                "[test_runner] '{}' iter {} LLM error (attempt {}/3): {e}",
                                test.id, iteration, attempt + 1
                            );
                            last_err = e;
                        }
                    }
                }
                match raw_opt {
                    Some(pair) => pair,
                    None => break 'react finalize_early(
                        ctx, run_id, test, &history,
                        format!("LLM error after 3 attempts: {last_err}")
                    ).await,
                }
            };
            // Trace stamp helper — captures the LLM metadata for THIS turn so
            // every history.push below can attach it without re-plumbing args.
            // Takes `parse_errors` by value to avoid borrowing the local
            // `parse_error_count` (mutated below in the parse-error branch).
            let llm_model = llm_meta.model.clone();
            let llm_latency_ms = llm_meta.latency_ms;
            let kb_hits_now = cached_kb_hits(&test.id);
            let trace_stamp = |step: &mut TestStep, parse_errors: u32| {
                step.llm_model = Some(llm_model.clone());
                step.llm_latency_ms = Some(llm_latency_ms);
                step.parse_errors = Some(parse_errors);
                step.kb_hits = kb_hits_now.clone();
                step.ts = Some(chrono::Local::now().to_rfc3339());
            };

            let step = parse_step(&raw);
            if let Some(err) = &step.parse_error {
                parse_error_count += 1;
                if parse_error_count >= max_parse_errors {
                    let mut s = TestStep {
                        thought: step.thought.clone(),
                        action: json!({"error": "invalid_json"}),
                        observation: format!("ERROR: {err}"),
                        ..Default::default()
                    };
                    trace_stamp(&mut s, parse_error_count);
                    history.push(s);
                    if let Some(rid) = run_id {
                        runs::push_observation(rid, format!("ERROR (parse): {err}\nRaw: {raw}"));
                    }
                    break 'react finalize_early(
                        ctx, run_id, test, &history,
                        format!("too many invalid LLM responses ({max_parse_errors})"),
                    ).await;
                }
                // Reprompt — save hint for next iteration.  The schema example
                // is critical: empirically Gemini drifts into "thought: ...\n
                // action_input: {...}\ndone: false" plain-text format after a
                // single parse error and never recovers without an explicit
                // template (root cause of run_20260425_210107_508_0 failing
                // 5/5 retries with the same pattern).
                parse_error_hint = Some(format!(
                    "⚠️ Previous response could not be parsed as JSON ({err}). \
                     Output STRICTLY one JSON object, no markdown fences, no prose before/after. \
                     The exact required shape is:\n\
                     {{\"thought\": \"<reasoning>\", \"action_input\": {{\"action\": \"<name>\", ...args}}, \"done\": false}}\n\
                     Do NOT write `thought: ...` or `action_input: ...` as plain text labels — \
                     they MUST be JSON keys inside one outer {{...}} object."
                ));
                if let Some(rid) = run_id {
                    runs::push_observation(rid, format!("PARSE_RETRY ({parse_error_count}/{max_parse_errors}): {err}\nRaw: {raw}"));
                }
                continue;  // don't push anything to visible history — LLM just retries
            }

            if step.done {
                let analysis = evaluate_success(ctx, test, &history, step.final_answer.clone()).await;
                let cap = if analysis.passed {
                    ScreenshotCapture { path: None, error: None }
                } else {
                    capture_screenshot(ctx, &test.id, run_id).await
                };
                break 'react TestResult {
                    test_id: test.id.clone(),
                    status: if analysis.passed { TestStatus::Passed } else { TestStatus::Failed },
                    iterations: iteration + 1,
                    duration_ms: started.elapsed().as_millis() as u64,
                    error_message: if analysis.passed { None } else { Some(analysis.reason.clone()) },
                    screenshot_path: cap.path,
                    screenshot_error: cap.error,
                    history,
                    final_analysis: Some(analysis.reason),
                    dispute: None,
                };
            }

            // Execute the browser tool call
            let mut action_input = step.action_input.clone();
            inject_session(&mut action_input, session_id);
            let action_label = action_input.get("action").and_then(Value::as_str).unwrap_or("?").to_string();
            if let Some(rid) = run_id {
                runs::set_phase(rid, runs::RunPhase::Running {
                    step: (iteration + 1),
                    current_action: action_label.clone(),
                });
            }
            // Issue #75: refresh the in-page action indicator with the
            // current action.  No-op when `show_action_indicator` is false.
            // Done in spawn_blocking — `with_tab` inside takes a Mutex.
            {
                let label = format!("{} ({}/{})", action_label, iteration + 1, max_iter);
                let _ = tokio::task::spawn_blocking(move || {
                    crate::browser::show_action_indicator(&label);
                }).await;
            }
            // Issue #103: `dispute_yaml` — LLM signals that the YAML spec has
            // a bug.  Terminate the loop immediately with Disputed status.
            // Side-effects: KB write + gh issue create (both fire-and-forget).
            if action_label == "dispute_yaml" {
                let reason = action_input
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("(no reason provided)")
                    .to_string();
                let suspected_step = action_input
                    .get("suspected_step")
                    .and_then(Value::as_i64);
                let suggested_fix = action_input
                    .get("suggested_fix")
                    .and_then(Value::as_str)
                    .map(String::from);
                let dispute_info = DisputeInfo { reason: reason.clone(), suspected_step, suggested_fix };

                // Capture URL for context (best-effort; ignore errors).
                let final_url = ctx.call_tool("web_navigate", json!({"action":"url"})).await
                    .ok()
                    .and_then(|v| v.get("url").and_then(Value::as_str).map(String::from))
                    .unwrap_or_default();

                let dispute_out = handle_dispute_yaml(
                    &test.id,
                    run_id,
                    iteration,
                    dispute_info,
                    &final_url,
                    &llm_model,
                );

                // Push a trace step so the dispute is visible in the history.
                let obs = format!("[dispute_yaml] reason: {reason}");
                if let Some(rid) = run_id {
                    runs::push_observation(rid, obs.clone());
                }
                let mut s = TestStep {
                    thought: step.thought,
                    action: action_input.clone(),
                    observation: obs,
                    ..Default::default()
                };
                trace_stamp(&mut s, parse_error_count);
                history.push(s);

                let cap = capture_screenshot(ctx, &test.id, run_id).await;
                break 'react TestResult {
                    test_id: test.id.clone(),
                    status: TestStatus::Disputed,
                    iterations: iteration + 1,
                    duration_ms: started.elapsed().as_millis() as u64,
                    error_message: Some(format!("dispute_yaml: {}", dispute_out.reason)),
                    screenshot_path: cap.path,
                    screenshot_error: cap.error,
                    history,
                    final_analysis: Some(format!("LLM disputed YAML spec at iteration {iteration}")),
                    dispute: Some(dispute_out),
                };
            }

            // Dispatch to the appropriate tool.  `expand_observation` is a
            // meta-tool (reads run registry, no browser action).  Everything else
            // goes through `web_navigate`.
            let raw_result = if action_label == "expand_observation" {
                ctx.call_tool("expand_observation", action_input.clone()).await
            } else {
                ctx.call_tool("web_navigate", action_input.clone()).await
            };
            let full_obs = match &raw_result {
                Ok(v) => v.to_string(),
                Err(e) => format!("ERROR: {e}"),
            };

            // Mid-loop black screen guard: if a screenshot action returns an all-
            // black image, Chrome likely crashed and recovered in headless mode
            // after the initial navigate check passed.  Re-navigate + tell LLM.
            if matches!(action_label.as_str(), "screenshot" | "screenshot_analyze") {
                if let Ok(ss_val) = &raw_result {
                    if is_all_black_screenshot(ss_val) {
                        tracing::warn!(
                            "[test_runner] ⚠️  '{}' iter {} — mid-loop black screen. \
                             Chrome crashed again; resetting + re-navigating.",
                            test.id, iteration
                        );
                        let wh = want_headless;
                        let nav_clone = nav_url.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            crate::browser::close();
                            crate::browser::set_test_headless_mode(wh);
                            crate::browser::ensure_open(wh)
                        }).await;
                        let mut nav_retry = json!({"action": "goto", "target": &nav_clone});
                        inject_session(&mut nav_retry, session_id);
                        let re_obs = ctx.call_tool("web_navigate", nav_retry).await
                            .map(|v| v.to_string())
                            .unwrap_or_else(|e| format!("re-navigate error: {e}"));
                        let recovery_obs = format!(
                            "⚠️ 螢幕全黑（Chrome 在 headless 模式下重啟）。已強制重開並重新導航至 {}。\
                             請重新執行 semantics bootstrap（eval flt-semantics-placeholder click）再繼續。\
                             重導航結果: {}",
                            nav_clone,
                            &re_obs[..re_obs.len().min(300)],
                        );
                        if let Some(rid) = run_id {
                            runs::push_observation(rid, recovery_obs.clone());
                        }
                        let mut s = TestStep {
                            thought: step.thought,
                            action: action_input,
                            observation: recovery_obs,
                            ..Default::default()
                        };
                        trace_stamp(&mut s, parse_error_count);
                        history.push(s);
                        continue;  // next iteration — LLM will see recovery message
                    }
                }
            }

            // Store full observation before truncation
            if let Some(rid) = run_id {
                runs::push_observation(rid, full_obs.clone());
            }
            let obs_for_llm = truncate_with_hint(&full_obs, history.len());

            // Convergence guard (signature path): build a compact signature
            // from the action input.  Skip "noise" actions (wait, sleep, a11y
            // bootstrap) so the rolling window captures meaningful intent.
            // If the same signature shows up LOOP_THRESHOLD times in the
            // rolling LOOP_WINDOW, the LLM is stuck retrying the same thing
            // despite identical feedback.
            let sig = action_signature(&action_input);
            if !is_noise_action(&action_input) {
                recent_action_sigs.push_back(sig.clone());
                if recent_action_sigs.len() > LOOP_WINDOW {
                    recent_action_sigs.pop_front();
                }
                let same_count = recent_action_sigs.iter().filter(|s| **s == sig).count();
                if same_count >= LOOP_THRESHOLD {
                    tracing::warn!(
                        "[test_runner] '{}' iter {}: convergence guard tripped — \
                         action `{}` repeated {}× in last {} non-noise steps; aborting early",
                        test.id, iteration, sig, same_count, recent_action_sigs.len()
                    );
                    // Best-effort KB raw note (fire-and-forget; never blocks).
                    record_guard_fire_to_kb(
                        test.id.clone(),
                        iteration,
                        "convergence",
                        sig.clone(),
                        full_obs.clone(),
                    );
                    let mut s = TestStep {
                        thought: step.thought,
                        action: action_input,
                        observation: obs_for_llm,
                        ..Default::default()
                    };
                    trace_stamp(&mut s, parse_error_count);
                    history.push(s);
                    break 'react finalize_early(
                        ctx, run_id, test, &history,
                        format!(
                            "convergence guard: action `{}` repeated {}× in {} non-noise steps — LLM stuck",
                            sig, same_count, recent_action_sigs.len()
                        ),
                    ).await;
                }
            }

            // Convergence guard (error-ratio path): track whether the last
            // LOOP_WINDOW observations were errors.  ≥ ERROR_RATIO_NUM/8
            // failures in window → abort.  Catches the "vary the action but
            // nothing works" pattern that signature-counting misses (e.g.
            // service test iter 13/14/17 — different shadow_click targets,
            // all returning "no matching element").  Excludes noise actions
            // so the ratio reflects substantive failure rate.
            if !is_noise_action(&action_input) {
                let was_error = is_error_observation(&full_obs);
                recent_obs_was_error.push_back(was_error);
                if recent_obs_was_error.len() > LOOP_WINDOW {
                    recent_obs_was_error.pop_front();
                }
                let err_count = recent_obs_was_error.iter().filter(|x| **x).count();
                if recent_obs_was_error.len() >= LOOP_WINDOW
                    && err_count >= ERROR_RATIO_NUM
                {
                    tracing::warn!(
                        "[test_runner] '{}' iter {}: error-ratio guard tripped — \
                         {}/{} of last actions failed; aborting early",
                        test.id, iteration, err_count, recent_obs_was_error.len()
                    );
                    // Best-effort KB raw note (fire-and-forget; never blocks).
                    record_guard_fire_to_kb(
                        test.id.clone(),
                        iteration,
                        "error-ratio",
                        sig.clone(),
                        full_obs.clone(),
                    );
                    let mut s = TestStep {
                        thought: step.thought,
                        action: action_input,
                        observation: obs_for_llm,
                        ..Default::default()
                    };
                    trace_stamp(&mut s, parse_error_count);
                    history.push(s);
                    break 'react finalize_early(
                        ctx, run_id, test, &history,
                        format!(
                            "error-ratio guard: {}/{} of last non-noise actions returned errors — page state likely broken",
                            err_count, recent_obs_was_error.len()
                        ),
                    ).await;
                }
            }

            let mut s = TestStep {
                thought: step.thought,
                action: action_input.clone(),
                observation: obs_for_llm,
                ..Default::default()
            };
            trace_stamp(&mut s, parse_error_count);
            history.push(s);

            // Issue #78: capture a timeline frame after each successful step.
            // Reuses crate::browser::screenshot which auto-applies the privacy
            // mask (Issue #80) — never bypass.  Errors are logged & ignored;
            // a half-recorded GIF is still valuable.
            if record_timeline {
                let target = action_input
                    .get("target")
                    .or_else(|| action_input.get("ref_id"))
                    .or_else(|| action_input.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Ok(Ok(bytes)) = tokio::task::spawn_blocking(crate::browser::screenshot).await {
                    timeline.push(crate::test_runner::gif_recorder::TimelineFrame {
                        step: iteration + 1,
                        action: action_label.clone(),
                        target,
                        png_bytes: bytes,
                    });
                }
            }
        }

        // Loop exhausted
        let cap = capture_screenshot(ctx, &test.id, run_id).await;
        TestResult {
            test_id: test.id.clone(),
            status: TestStatus::Failed,
            iterations: max_iter,
            duration_ms: started.elapsed().as_millis() as u64,
            error_message: Some(format!("max iterations ({max_iter}) reached without DONE")),
            screenshot_path: cap.path,
            screenshot_error: cap.error,
            history,
            final_analysis: None,
            dispute: None,
        }
    };  // end 'react block

    // 4) Fixture cleanup — always runs regardless of test pass/fail/timeout/error.
    if let Some(fixture) = &test.fixture {
        for step in &fixture.cleanup {
            if let Err(e) = run_fixture_step(ctx, step, session_id).await {
                tracing::warn!("[fixture] cleanup step '{}' failed: {e}", step.action);
            }
        }
    }

    // Issue #78: on failure, encode the buffered frames into timeline.gif.
    // Soft-fail — never blocks triage.  Single-frame screenshot path stays.
    if record_timeline
        && !matches!(run_result.status, TestStatus::Passed)
        && !timeline.is_empty()
    {
        if let Some(rid) = run_id {
            let path = crate::test_runner::gif_recorder::timeline_gif_path(rid);
            match timeline.encode_to_gif(&path) {
                Ok(_) => tracing::info!(
                    "[test_runner] timeline GIF saved: {} ({} frames)",
                    path.display(), timeline.len()
                ),
                Err(e) => tracing::warn!("[test_runner] timeline GIF encode failed: {e}"),
            }
        }
    }

    run_result
}

struct ScreenshotCapture {
    path: Option<String>,
    error: Option<String>,
}

async fn finalize_early(
    ctx: &crate::adk::context::AgentContext,
    run_id: Option<&str>,
    test: &TestGoal,
    history: &[TestStep],
    msg: String,
) -> TestResult {
    let cap = capture_screenshot(ctx, &test.id, run_id).await;
    TestResult {
        test_id: test.id.clone(),
        status: TestStatus::Error,
        iterations: history.len() as u32,
        duration_ms: 0,
        error_message: Some(msg),
        screenshot_path: cap.path,
        screenshot_error: cap.error,
        history: history.to_vec(),
        final_analysis: None,
        dispute: None,
    }
}

/// Capture a screenshot, save to disk AND store bytes to run registry if
/// `run_id` is set.  Surface any error (spawn_blocking failure, CDP error,
/// filesystem write error).
async fn capture_screenshot(
    ctx: &crate::adk::context::AgentContext,
    test_id: &str,
    run_id: Option<&str>,
) -> ScreenshotCapture {
    // Tell the tool (publishes event for UI)
    let _ = ctx.call_tool("web_navigate", json!({"action": "screenshot"})).await;

    let bytes_result: Result<Vec<u8>, String> = tokio::task::spawn_blocking(
        crate::browser::screenshot
    ).await
    .map_err(|e| format!("spawn_blocking failed: {e}"))
    .and_then(|r| r);

    match bytes_result {
        Ok(bytes) => {
            if let Some(rid) = run_id {
                crate::test_runner::runs::set_screenshot(rid, Ok(bytes.clone()));
            }
            let failures_dir = crate::platform::app_data_dir().join("test_failures");
            let path = failures_dir.join(format!("{test_id}_{}.png",
                chrono::Local::now().format("%Y%m%d_%H%M%S")));
            if let Err(e) = std::fs::create_dir_all(&failures_dir) {
                let msg = format!("mkdir failed: {e}");
                return ScreenshotCapture { path: None, error: Some(msg) };
            }
            if let Err(e) = std::fs::write(&path, &bytes) {
                let msg = format!("write {:?} failed: {e}", path);
                return ScreenshotCapture { path: None, error: Some(msg) };
            }
            ScreenshotCapture { path: Some(path.to_string_lossy().to_string()), error: None }
        }
        Err(e) => {
            if let Some(rid) = run_id {
                crate::test_runner::runs::set_screenshot(rid, Err(e.clone()));
            }
            ScreenshotCapture { path: None, error: Some(e) }
        }
    }
}

/// Truncate observation for LLM history, appending a retrieval hint if cut.
fn truncate_with_hint(full: &str, step_idx: usize) -> String {
    let char_count = full.chars().count();
    if char_count <= OBS_TRUNCATE_CHARS { return full.to_string(); }
    let head: String = full.chars().take(OBS_TRUNCATE_CHARS).collect();
    format!(
        "{head}... [truncated: full length {char_count} chars. \
         Use MCP get_full_observation(run_id, step={step_idx}) to fetch complete content.]"
    )
}

// ── Prompt building ──────────────────────────────────────────────────────────

/// Full prompt — all history with adaptive observation truncation.
/// - Default: 500-char observations for balanced token usage
/// - Vision-heavy tests: use OBS_TRUNCATE_CHARS_VISION_HEAVY (500 chars) for aggressive savings
/// Used by Gemini / main LLM backend.
fn build_prompt(test: &TestGoal, history: &[TestStep], parse_error_hint: Option<&str>) -> String {
    // Detect if this test requires frequent vision analysis (multiple screenshot_analyze calls)
    let vision_call_count = history
        .iter()
        .filter(|step| {
            step.observation.contains("__vision") || 
            (step.action.get("action").is_some_and(|a| a.as_str() == Some("screenshot_analyze")))
        })
        .count();
    let is_vision_heavy = vision_call_count >= 3; // 3+ vision calls → aggressive truncation
    
    let obs_limit = if is_vision_heavy {
        OBS_TRUNCATE_CHARS_VISION_HEAVY
    } else {
        500
    };
    build_prompt_with_limits(test, history, parse_error_hint, usize::MAX, obs_limit)
}

/// Vision-mode prompt: the screenshot is the primary observation, so we
/// keep the text portion lean — last 3 history steps, 200-char observations,
/// no AX-tree dump, no sprawling action catalogue.  Tells the LLM it is
/// looking at the current viewport and should prefer pixel-based actions.
fn build_prompt_vision(
    test: &TestGoal,
    history: &[TestStep],
    parse_error_hint: Option<&str>,
    perception: &crate::perception::PagePerception,
) -> String {
    let skipped = history.len().saturating_sub(3);
    let visible = &history[skipped..];

    let history_str = if visible.is_empty() && skipped == 0 {
        "(none yet)".to_string()
    } else {
        let prefix = if skipped > 0 {
            format!(
                "[{skipped} earlier step(s) omitted — showing last {} for context]\n---\n",
                visible.len()
            )
        } else {
            String::new()
        };
        let steps: String = visible
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let obs = truncate(&s.observation, 200);
                let step_n = skipped + i + 1;
                format!(
                    "[Step {step_n}]\nThought: {}\nAction: {}\nObservation: {obs}\n",
                    s.thought, s.action
                )
            })
            .collect::<Vec<_>>()
            .join("---\n");
        format!("{prefix}{steps}")
    };

    let hint_block = parse_error_hint
        .map(|h| format!("\n## ⚠️ Reprompt notice\n{h}\n"))
        .unwrap_or_default();

    let locale = crate::test_runner::i18n::Locale::from_yaml(&test.locale);
    let criteria = if test.success_criteria.is_empty() {
        locale.default_criteria().to_string()
    } else {
        test.success_criteria
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let docs_block = docs_prompt_block(&test.id);

    format!(
        r##"You are a browser-testing agent driving a live Chrome window.  The attached image is a PNG screenshot of the **current viewport** — treat it as the primary observation.

## Goal
{goal}

## Page context
{summary}
{docs_block}
## Success criteria
{criteria}

## Preferred actions (canvas-safe; call via tool "web_navigate", field "action")
- click_point    — x, y: pixel coords read directly off the screenshot PNG.  Use this as the default for Flutter / canvas apps.  ⚠️ HiDPI / Retina screens: ALWAYS pass `"coord_source": "screenshot"` so we divide by devicePixelRatio — without it your click lands at 2× / 4× the wrong place.
- type           — target: CSS selector (if any), text: text to type.  On canvas pages without selectors, prefer click_point on the input first, then `type` with target omitted.
- key            — target: key name (Enter / Tab / Escape)
- scroll         — x, y: pixel deltas (default 0, 300)
- goto           — target: URL
- wait           — target: CSS selector OR ms number
- screenshot     — re-capture the viewport (next turn's image will reflect the new state)
- eval           — target: JS expression (use sparingly — only when coordinates won't do)

## Accessibility tree (use only when you need EXACT text — e.g. number assertions)
- enable_a11y, ax_find, ax_value, ax_click, ax_type

## History
{history}
{hint}
## Instructions
Look at the attached screenshot.  Decide the single next action.  Prefer
click_point with coordinates you read off the image — do NOT invent a CSS
selector when you have pixels.  When the goal is clearly achieved (or
definitively failed), set "done": true.

Respond with STRICTLY valid JSON, no markdown fences, no prose:
{{
  "thought": "<reasoning in {lang} — cite what you see in the screenshot>",
  "action_input": {{ "action": "click_point", "x": 640, "y": 420, "coord_source": "screenshot" }},
  "done": false,
  "final_answer": "<only when done=true: summary of outcome in {lang}>"
}}
"##,
        goal = test.goal.trim(),
        summary = perception.summary,
        docs_block = docs_block,
        criteria = criteria,
        history = history_str,
        hint = hint_block,
        lang = locale.reasoning_language(),
    )
}

/// Compact prompt for `claude_cli` backend.
///
/// Keeps only the last 3 history steps and truncates observations to 200 chars,
/// preventing the 10-20 KB prompt that causes `claude -p` to hang on iteration 2+
/// (observed 2026-04-20: full prompt with screenshot-analysis history reliably
/// hit the 600 s subprocess watchdog; compact prompt completes in ~6 s).
fn build_prompt_compact(test: &TestGoal, history: &[TestStep], parse_error_hint: Option<&str>) -> String {
    build_prompt_with_limits(test, history, parse_error_hint, 3, 200)
}

fn build_prompt_with_limits(
    test: &TestGoal,
    history: &[TestStep],
    parse_error_hint: Option<&str>,
    max_history_steps: usize,
    obs_max_chars: usize,
) -> String {
    // Trim history to the last N steps if needed.
    let skipped = history.len().saturating_sub(max_history_steps);
    let visible = &history[skipped..];

    let history_str = if visible.is_empty() && skipped == 0 {
        "(none yet)".to_string()
    } else {
        let prefix = if skipped > 0 {
            format!(
                "[{skipped} earlier step(s) omitted — showing last {} for context]\n---\n",
                visible.len()
            )
        } else {
            String::new()
        };
        let steps: String = visible.iter().enumerate().map(|(i, s)| {
            let obs = truncate(&s.observation, obs_max_chars);
            let step_n = skipped + i + 1;
            format!("[Step {step_n}]\nThought: {}\nAction: {}\nObservation: {obs}\n",
                s.thought, s.action)
        }).collect::<Vec<_>>().join("---\n");
        format!("{prefix}{steps}")
    };

    let hint_block = parse_error_hint
        .map(|h| format!("\n## ⚠️ Reprompt notice\n{h}\n"))
        .unwrap_or_default();

    let locale = crate::test_runner::i18n::Locale::from_yaml(&test.locale);
    let criteria = if test.success_criteria.is_empty() {
        locale.default_criteria().to_string()
    } else {
        test.success_criteria.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n")
    };

    let docs_block = docs_prompt_block(&test.id);

    format!(r##"You are a browser-testing agent.  Your job is to achieve the test goal by driving the browser.

## Goal
{goal}

## Test URL (already opened)
{url}
{docs_block}
## Success criteria
{criteria}

## Available browser actions (call via tool "web_navigate", field "action")
- goto           — target: URL
- screenshot     — capture page PNG
- dom_snapshot   — ⭐ CiC-style stable element refs for standard HTML pages (Issue #74).
                   Returns {{url, count, truncated, elements:[{{ref:"e3",role,name,tag,bbox}}, …]}}.
                   Pass `ref_id: "e3"` to click/type/read/exists/hover instead of `target` —
                   the ref survives re-renders better than a brittle CSS selector.
                   Optional `max` param caps element count (default 200, sets `truncated:true`).
                   ⚠️ ref_ids reset after `goto` — re-snapshot after navigation.
                   ⚠️ Flutter / canvas pages: prefer shadow_*/ax_* (this targets standard HTML).
- click          — target: CSS selector OR plain text label (XPath text search); OR pass `ref_id` from dom_snapshot
- type           — target: CSS selector (or `ref_id`), text: input text
- read           — target: CSS selector (or `ref_id`) → returns innerText
- eval           — target: JS expression → returns result
- wait           — target: CSS selector (waits for element) OR plain ms number (sleeps, e.g. "2000")
- exists         — target: CSS selector (or `ref_id`) → true/false
- attr           — target: selector, text: attribute name
- scroll         — x, y: pixels (default 0, 300)
- scroll_to      — target: selector
- click_point    — x, y: viewport pixel coords; use for Flutter/CanvasKit canvas apps where CSS selectors don't work
- key            — target: key name (Enter/Tab/Escape)
- screenshot_analyze — target: question for vision LLM about the page
- console        — return captured console messages
- network        — return captured fetch/XHR

## Accessibility tree actions (literal text, no vision approximation)
For Flutter/CanvasKit canvas apps AND exact-string assertions:
- enable_a11y       — ⚠️ MUST call first on Flutter/CanvasKit apps; without it ax_find/shadow_find
                       return empty because the semantics bridge is inactive. Call again after
                       any route change (tree collapses temporarily after navigation).
- ax_tree           — list all a11y nodes (role + literal name + value + backend_id)
- ax_find           — role and/or name (substring, case-insensitive); optional name_regex for EXACT match
                       (e.g. name_regex="^登入$" to match only "登入" and not "使用 Google 登入");
                       not_name_matches=[...] array to exclude by substring; returns single match.
                       ⚠️ Use name_regex="^<exact>$" when the target name is a substring of other node names.
- ax_value          — backend_id → exact text (value || name)
- ax_click          — backend_id → click via DOM box model centre (Flutter-compatible 5-event sequence)
- ax_focus          — backend_id → DOM focus
- ax_type           — backend_id, text → focus + insertText
- ax_type_verified  — same as ax_type + read-back; returns {{typed, actual, matched}}

## Flutter Shadow DOM actions (⭐ PREFERRED for Flutter/CanvasKit — bypasses CDP AX protocol)
These query Flutter's `flt-semantics-host` directly via JS, avoiding AX tree collapse issues:
- shadow_dump           — list ALL elements in Flutter shadow DOM (role:label pairs); use first to debug
- shadow_find           — role and/or name_regex → {{found, x, y, label}}; params: role, name_regex (or name)
- shadow_click          — same params as shadow_find; clicks via JS PointerEvent dispatch
                          (NOT CDP Input.dispatchMouseEvent — that causes about:blank on Flutter nav buttons)
- shadow_type           — role + name_regex + text; clicks to focus then inserts text via CDP InsertText
- flutter_type          — ASCII text only; fires CDP keydown per character (REQUIRED for Flutter textboxes).
                          Call shadow_click + wait 350ms first to focus the field, THEN flutter_type.
                          ⚠️ Input.InsertText does NOT work for Flutter — always use flutter_type.
                          ⚠️ ASCII only — CJK/Unicode chars (你好等) have no keycode and will fail.
                          Use shadow_type for non-ASCII text (but note InsertText may not update Flutter state).
- flutter_enter         — no params; sends Enter key to the active flt-text-editing input.
                          Use immediately after flutter_type to submit a chat message or form.
                          ⚠️ More reliable than shadow_click on icon-only unlabeled send buttons.
- shadow_type_flutter   — all-in-one: shadow_click → wait 350ms → flutter_type; preferred for textboxes.
                          params: role, name_regex (or name), text

Flutter/CanvasKit interaction pattern (PREFERRED order using shadow DOM):
  1. enable_a11y                — trigger Flutter to build semantics overlay
  2. shadow_dump                — inspect what's available (first call on each page)
  3. shadow_click               — for buttons and tabs
  4. shadow_type_flutter        — for text input fields (NOT shadow_type which uses InsertText)
  After route change: wait ≥ 1000ms → enable_a11y → shadow_dump → interact.

**常見錯誤（會導致 30 秒 timeout）：**
❌ shadow_click → wait 3000 → wait_for_ax_ready  （缺 enable_a11y → timeout）
✓ shadow_click → wait 2000 → enable_a11y → wait_for_ax_ready  （正確）

每次 Flutter route 切換後都要重新 enable_a11y。
enable_a11y 必須在 wait_for_ax_ready 之前，否則語義樹永遠不會被初始化。

Fallback to ax_find/ax_click if shadow_find returns "no shadow root".

When you need EXACT text comparison (numbers, IDs), prefer ax_* over
screenshot_analyze (which approximates).

## 當 shadow_dump 找不到目標時的替代策略（重要）

連續 2 次 shadow_dump 仍找不到目標元素時，立刻切換到 Vision-Coordinate 策略：

1. screenshot_analyze "找 <目標元素> 的螢幕位置，估計中心 x, y 座標（viewport pixel）"
2. click_point x=<估計值> y=<估計值> coord_source=screenshot

⚠️ 不要在同一頁面 shadow_dump 超過 2 次。
截圖可以直接看到 Flutter canvas 上的 UI，比 AX tree 更直接。
click_point coord_source=screenshot 會自動修正 HiDPI 縮放。

## Robustness actions (test isolation + race-free)
- go_back        — browser history back 1 step + waits for Flutter AX tree to settle (≥10 nodes, 8 s)
                   optional param: wait (extra ms after AX ready, e.g. {{"action":"go_back","wait":2000}})
                   ⭐ USE THIS to exit Flutter pushed routes (e.g. product-edit → product-list)
                   instead of goto (full reload) or shadow_click bottom-nav (unreachable in pushed route)
- clear_state    — wipe cookies / localStorage / sessionStorage / IndexedDB / caches
                   (call between tests to prevent cross-test leakage)
- wait_new_tab   — block until a new tab opens; param: timeout (ms, default 10000)
                   (use after clicking OAuth / popup buttons)
- wait_request   — block until a network request matching `target` (URL substring)
                   appears in the capture; param: timeout (ms, default 10000)
                   (auto-installs network capture; eliminates "click then read"
                   race conditions before asserting on request body)

## Separate tool: expand_observation
When a previous Observation was truncated (you'll see "[truncated: ...]"),
you can fetch the complete content by outputting an action that calls the
`expand_observation` tool directly (not via web_navigate):
  {{"action": "expand_observation", "step": N}}
Where N is the 0-indexed step number from the truncation hint.

## dispute_yaml — 回報 YAML spec 問題（標準動作，非失敗）
當你發現 YAML spec 本身有問題時，呼叫：
  {{"action": "dispute_yaml", "reason": "<原因>", "suspected_step": <步驟編號或 null>, "suggested_fix": "<修正建議或 null>"}}

**何時呼叫 dispute_yaml：**
1. 你執行某 step 後預期 page 變化，但畫面沒變 → 先 retry 1 次再 dispute
2. screenshot 顯示你**已經在 step 預設要去的目的地**（例如 step 2 說「切到商品頁」但你已在商品頁）
3. element name_regex 跟畫面上實際 button label 明顯不符
4. step 順序邏輯不通（例如 step N 需要 step M 的產出但 M 沒做到）

**不要 dispute：**
- 因為 element 暫時找不到（先 retry 1 次）
- 因為 LLM 自己判斷不出來（dispute 要明確指出哪個 step 哪裡錯）

dispute_yaml 不是失敗——是回報 spec 問題的標準動作。Sirin 會自動開 GitHub issue 給維護者。

## History
{history}
{hint}
## Instructions
Analyse the goal and history, decide the single next action.
When the goal is clearly achieved (or definitively failed), set "done": true.

Respond with STRICTLY valid JSON, no markdown fences, no prose:
{{
  "thought": "<reasoning in {lang}>",
  "action_input": {{ "action": "click", "target": "#btn" }},
  "done": false,
  "final_answer": "<only when done=true: summary of outcome in {lang}>"
}}
"##,
        goal = test.goal.trim(),
        url = test.url,
        docs_block = docs_block,
        criteria = criteria,
        history = history_str,
        hint = hint_block,
        lang = locale.reasoning_language(),
    )
}

// ── Step parsing ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ParsedStep {
    thought: String,
    action_input: Value,
    done: bool,
    final_answer: Option<String>,
    parse_error: Option<String>,
}

/// Resolve which LLM backend to use for this test.
/// Precedence: per-test YAML field → `TEST_RUNNER_LLM_BACKEND` env → default ("").
/// Default ("" / unrecognized) means: use Sirin's main LLM config
/// (`call_coding_prompt`).
fn resolve_llm_backend(test: &TestGoal) -> String {
    if let Some(b) = test.llm_backend.as_deref() {
        let trimmed = b.trim();
        if !trimmed.is_empty() {
            return trimmed.to_lowercase();
        }
    }
    std::env::var("TEST_RUNNER_LLM_BACKEND")
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// Dispatch the next ReAct prompt to the right LLM backend based on test config.
///
/// Accepts raw `history` + `parse_error_hint` so it can build the appropriate
/// prompt variant per backend:
/// - `claude_cli` / `claude` → compact prompt (last 3 steps, 200-char obs) to
///   stay well under the ~10 KB threshold that causes `claude -p` to hang.
/// - anything else → full prompt via Sirin's main LLM config.
/// Trace metadata returned alongside the raw LLM response so the ReAct loop
/// can stamp it onto the resulting [`TestStep`].
pub(crate) struct LlmCallMeta {
    pub model: String,
    pub latency_ms: u64,
}

async fn call_test_llm(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
    history: &[TestStep],
    parse_error_hint: Option<&str>,
    perception: &crate::perception::PagePerception,
) -> Result<(String, LlmCallMeta), String> {
    let started = std::time::Instant::now();
    // Vision path: attach screenshot as primary observation.  Still requires
    // the resolved mode to be Vision AND the capture to have succeeded; if
    // the screenshot failed (None), we gracefully fall back to text prompt.
    if matches!(
        perception.resolved_mode,
        crate::perception::PerceptionMode::Vision
    ) {
        if let Some(b64) = perception.screenshot_b64.as_deref() {
            let prompt = build_prompt_vision(test, history, parse_error_hint, perception);
            let model = format!("vision:{}", ctx.llm.model);
            let res = crate::llm::call_vision(
                ctx.http.as_ref(),
                ctx.llm.as_ref(),
                &prompt,
                b64,
                "image/png",
            )
            .await
            .map_err(|e| e.to_string());
            return res.map(|s| (s, LlmCallMeta {
                model,
                latency_ms: started.elapsed().as_millis() as u64,
            }));
        }
        tracing::warn!(
            "[test_runner] perception=vision requested for '{}' but screenshot unavailable; \
             falling back to text prompt",
            test.id
        );
    }

    let backend = resolve_llm_backend(test);
    match backend.as_str() {
        "claude_cli" | "claude" => {
            let prompt = build_prompt_compact(test, history, parse_error_hint);
            call_claude_cli(prompt).await.map(|s| (s, LlmCallMeta {
                model: "claude_cli".into(),
                latency_ms: started.elapsed().as_millis() as u64,
            }))
        }
        _ => {
            // Auto-switch to a compact rolling-window prompt after 8 steps.
            // Gemini Flash 2.0 (and similar models) start producing invalid JSON
            // at ~15K tokens (≈ iteration 13+ with full observations).
            // Compact mode cuts the prompt to last 5 steps × 300-char obs,
            // keeping the goal + criteria intact.  More generous than the
            // claude_cli path (3 steps, 200 chars) since bandwidth isn't the
            // issue here — it's JSON drift from a very long context window.
            let prompt = if history.len() >= 8 {
                tracing::debug!(
                    "[test_runner] {} iter {}: switching to compact prompt (history={})",
                    test.id,
                    history.len(),
                    history.len(),
                );
                build_prompt_with_limits(test, history, parse_error_hint, 5, 300)
            } else {
                build_prompt(test, history, parse_error_hint)
            };
            let model = ctx.llm.effective_coding_model().to_string();
            crate::llm::call_coding_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt)
                .await
                .map_err(|e| e.to_string())
                .map(|s| (s, LlmCallMeta {
                    model,
                    latency_ms: started.elapsed().as_millis() as u64,
                }))
        }
    }
}

/// Spawn a `claude -p` subprocess and return its stdout as the LLM response.
///
/// Runs on a blocking task pool (claude CLI is a synchronous subprocess).
/// Uses the current working directory as `cwd` — the test_runner doesn't
/// need a specific repo context for browser-driving prompts.
async fn call_claude_cli(prompt: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".into());
        crate::claude_session::run_sync(&cwd, &prompt)
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))?
    .and_then(|r| {
        if r.success {
            Ok(r.output)
        } else {
            Err(format!("claude exit {}: {}", r.exit_code, r.output))
        }
    })
}

fn parse_step(raw: &str) -> ParsedStep {
    let cleaned = strip_fences(raw);
    match serde_json::from_str::<Value>(&cleaned) {
        Ok(v) => {
            let thought = v.get("thought").and_then(Value::as_str).unwrap_or_default().to_string();
            let mut action_input = v.get("action_input").cloned().unwrap_or(json!({}));
            let mut done = v.get("done").and_then(Value::as_bool).unwrap_or(false);
            let final_answer = v.get("final_answer").and_then(Value::as_str).map(String::from).filter(|s| !s.is_empty());

            // Recovery: when the LLM omits the {thought, action_input, done} wrapper
            // and writes the action JSON directly (or strip_fences extracted only the
            // inner action_input due to "thought: ...\naction_input: {...}\n" plain-
            // text format from Gemini), the parsed root will have an `action` field
            // but no `thought` / `action_input`.  Treat the root as action_input.
            //
            // This is the dominant Gemini failure mode observed on
            // run_20260425_210107_508_0 (5/5 PARSE_RETRY iterations all hit it).
            let root_has_action = v.get("action").and_then(Value::as_str).is_some();
            let root_missing_wrapper =
                v.get("thought").is_none() && v.get("action_input").is_none();
            if root_has_action && root_missing_wrapper {
                action_input = v.clone();
                // strip_fences may have discarded a trailing `done: true` line
                // that was outside the JSON object.  Recover it from raw.
                if !done && raw_indicates_done(raw) {
                    done = true;
                }
            }

            // Require action_input to include an "action" field unless done
            if !done && action_input.get("action").and_then(Value::as_str).is_none() {
                return ParsedStep {
                    thought, action_input, done, final_answer,
                    parse_error: Some("action_input missing 'action' field".into()),
                };
            }
            ParsedStep { thought, action_input, done, final_answer, parse_error: None }
        }
        Err(e) => {
            // Last-ditch: try the plain-text "thought: ...\naction_input: {...}\ndone: ..."
            // shape that Gemini sometimes produces on retries.  We pull out the
            // first `{...}` block as action_input and any boolean `done: true/false`.
            if let Some(parsed) = parse_plaintext_step(raw) {
                return parsed;
            }
            ParsedStep {
                parse_error: Some(format!("JSON parse: {e}")),
                ..Default::default()
            }
        }
    }
}

/// True for "noise" actions that shouldn't fill the convergence-guard window.
///
/// These are passive / setup actions a test author legitimately uses many
/// times in a row: `wait`, `sleep`, `enable_a11y` (Flutter bootstrap),
/// `screenshot` (read-only).  Letting them dilute the window would mask
/// stuck-loops where the LLM does "fail → wait → fail → wait" forever
/// (run_..._11 service test exhibited exactly this — guard didn't trip
/// because waits between failures kept signature counts under threshold).
fn is_noise_action(action_input: &Value) -> bool {
    let a = action_input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        a.as_str(),
        "wait" | "sleep" | "enable_a11y" | "screenshot" | "page_state" | "url" | "title"
    )
}

/// True when an observation indicates an action error.
///
/// We check both the textual prefix (the executor wraps action errors as
/// `ERROR: <msg>`) and a couple of common Result-shape JSON wrappers in case
/// downstream wrappers ever serialize them differently.
fn is_error_observation(obs: &str) -> bool {
    let trimmed = obs.trim_start();
    if trimmed.starts_with("ERROR") || trimmed.starts_with("Err(") {
        return true;
    }
    // JSON shape: {"error":"..."} or {"status":"failed"} variants.
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.get("error").is_some() {
            return true;
        }
        if let Some(s) = v.get("status").and_then(Value::as_str) {
            if matches!(s, "failed" | "error" | "timeout") {
                return true;
            }
        }
    }
    false
}

/// Build a compact signature from an `action_input` JSON for the convergence
/// guard: `<action>:<target_or_role>:<name_regex_or_text_truncated>`.
///
/// Two iterations sharing the same signature mean the LLM picked the same
/// action with the same primary inputs.  Used by the ReAct loop to detect
/// LLM stuck-loops (see `LOOP_THRESHOLD` in `run_test_inner`).
fn action_signature(action_input: &Value) -> String {
    let action = action_input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("?");
    // Pick the most discriminating secondary key per action family.
    let secondary = action_input
        .get("target")
        .or_else(|| action_input.get("role"))
        .or_else(|| action_input.get("backend_id"))
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            other            => Some(other.to_string()),
        })
        .unwrap_or_default();
    let tertiary = action_input
        .get("name_regex")
        .or_else(|| action_input.get("text"))
        .and_then(Value::as_str)
        .map(|s| {
            // Trim long fields so signatures stay compact + comparable.
            if s.chars().count() > 24 {
                s.chars().take(24).collect::<String>() + "…"
            } else {
                s.to_string()
            }
        })
        .unwrap_or_default();
    format!("{action}:{secondary}:{tertiary}")
}

/// True when the raw LLM output indicates `done = true` outside any JSON
/// object — used by the root-action recovery path because strip_fences may
/// have stripped a trailing `done: true` plain-text line.
fn raw_indicates_done(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    // "done: true" / "done : true" plain-text label
    if lower.contains("done: true") || lower.contains("done : true") {
        return true;
    }
    // JSON-shape "done":true / "done": true  (in case the JSON was preceded
    // by stray prose that broke top-level parsing)
    lower.contains("\"done\":true") || lower.contains("\"done\": true")
}

/// Recovery parser for the plain-text "label: value" shape Gemini sometimes
/// drifts into after a parse retry — e.g.:
///
/// ```text
/// thought: 我已經完成步驟 1...
/// action_input: {"action":"wait","target":1500}
/// done: false
/// ```
///
/// Returns `Some(ParsedStep)` only when an `action_input: {…}` block was
/// extractable AND its inner JSON has an `action` field.  Otherwise returns
/// `None` so the caller emits the original parse error.
fn parse_plaintext_step(raw: &str) -> Option<ParsedStep> {
    let t = raw.trim();
    // Locate "action_input:" or "action:" label (case-insensitive on the prefix).
    let label_idx = t
        .to_lowercase()
        .find("action_input")
        .or_else(|| t.to_lowercase().find("action:"))?;
    let after_label = &t[label_idx..];
    // Find the first '{' after the label.
    let brace_start = after_label.find('{')?;
    // Find the matching closing brace by simple depth counting (skip strings).
    let bytes = after_label.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut end_off: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate().skip(brace_start) {
        if escape { escape = false; continue; }
        match b {
            b'\\' if in_string => escape = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 { end_off = Some(i); break; }
            }
            _ => {}
        }
    }
    let end = end_off?;
    let json_blob = &after_label[brace_start..=end];
    let action_input: Value = serde_json::from_str(json_blob).ok()?;
    if action_input.get("action").and_then(Value::as_str).is_none() {
        return None;
    }
    Some(ParsedStep {
        thought: String::new(),
        action_input,
        done: raw_indicates_done(raw),
        final_answer: None,
        parse_error: None,
    })
}

fn strip_fences(raw: &str) -> String {
    let t = raw.trim();
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let after = after.trim_start_matches('\n');
        if let Some(end) = after.rfind("```") {
            return after[..end].trim().to_string();
        }
    }
    if let (Some(s), Some(e)) = (t.find('{'), t.rfind('}')) {
        if e > s { return t[s..=e].to_string(); }
    }
    t.to_string()
}

fn format_observation(v: &Value) -> String {
    truncate(&v.to_string(), 800)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() }
    else {
        let head: String = s.chars().take(max).collect();
        format!("{head}... [truncated]")
    }
}

// ── Success evaluation ───────────────────────────────────────────────────────

pub struct SuccessAnalysis {
    pub passed: bool,
    pub reason: String,
}

/// Ask the LLM to judge whether success criteria are met.
async fn evaluate_success(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
    history: &[TestStep],
    agent_final: Option<String>,
) -> SuccessAnalysis {
    let locale = crate::test_runner::i18n::Locale::from_yaml(&test.locale);
    let criteria = if test.success_criteria.is_empty() {
        locale.evaluate_default_criteria().to_string()
    } else {
        test.success_criteria.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n")
    };

    let history_summary = history.iter().enumerate()
        .map(|(i, s)| format!("{}. {} → {}", i + 1,
            s.action.to_string().chars().take(80).collect::<String>(),
            truncate(&s.observation, 120)))
        .collect::<Vec<_>>()
        .join("\n");

    // Grab current URL + page text hint
    let url = ctx.call_tool("web_navigate", json!({"action":"url"})).await
        .ok().and_then(|v| v.get("url").and_then(Value::as_str).map(String::from)).unwrap_or_default();

    let prompt = format!(r#"{header}

Goal: {goal}
Success criteria:
{criteria}

Execution history (summary):
{history}

Final URL: {url}
Agent final message: {agent_final}

{judgment_hint}
{{"passed": true/false, "reason": "<{lang} {reason_hint}>"}}
"#,
        header = locale.evaluate_prompt_header(),
        goal = test.goal.trim(),
        criteria = criteria,
        history = history_summary,
        url = url,
        agent_final = agent_final.unwrap_or_else(|| "(none)".into()),
        judgment_hint = locale.evaluate_judgment_hint(),
        lang = locale.reasoning_language(),
        reason_hint = locale.evaluate_reason_hint(),
    );

    let raw = match crate::llm::call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt).await {
        Ok(s) => s,
        Err(e) => return SuccessAnalysis { passed: false, reason: format!("evaluate LLM error: {e}") },
    };

    let cleaned = strip_fences(&raw);
    match serde_json::from_str::<Value>(&cleaned) {
        Ok(v) => SuccessAnalysis {
            passed: v.get("passed").and_then(Value::as_bool).unwrap_or(false),
            reason: v.get("reason").and_then(Value::as_str).unwrap_or("no reason").to_string(),
        },
        Err(_) => SuccessAnalysis { passed: false, reason: format!("unparseable judgment: {raw}") },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_step() {
        let raw = r##"{"thought":"go","action_input":{"action":"click","target":"#x"},"done":false}"##;
        let s = parse_step(raw);
        assert!(s.parse_error.is_none());
        assert_eq!(s.action_input["action"], "click");
        assert!(!s.done);
    }

    #[test]
    fn parse_done_step() {
        let raw = r#"{"thought":"ok","done":true,"final_answer":"logged in"}"#;
        let s = parse_step(raw);
        assert!(s.parse_error.is_none());
        assert!(s.done);
        assert_eq!(s.final_answer.as_deref(), Some("logged in"));
    }

    #[test]
    fn parse_rejects_missing_action() {
        let raw = r##"{"thought":"hmm","action_input":{"target":"#x"},"done":false}"##;
        let s = parse_step(raw);
        assert!(s.parse_error.is_some());
    }

    #[test]
    fn parse_strips_markdown_fences() {
        let raw = "```json\n{\"thought\":\"ok\",\"done\":true}\n```";
        let s = parse_step(raw);
        assert!(s.parse_error.is_none());
        assert!(s.done);
    }

    /// Recovery: LLM omits the {thought, action_input, done} wrapper and writes
    /// the action JSON directly at the root.  Previously this raised
    /// "action_input missing 'action' field" — now the root is treated as
    /// action_input.
    #[test]
    fn parse_recovers_root_action() {
        let raw = r#"{"action":"wait","target":1500}"#;
        let s = parse_step(raw);
        assert!(s.parse_error.is_none(), "got parse_error: {:?}", s.parse_error);
        assert_eq!(s.action_input["action"], "wait");
        assert_eq!(s.action_input["target"], 1500);
        assert!(!s.done);
    }

    /// Recovery: strip_fences extracted only the inner action_input JSON from
    /// a "thought: ...\naction_input: {...}\ndone: false" plaintext envelope —
    /// the parsed root therefore has `action` but no `thought`/`action_input`.
    /// This is the dominant Gemini failure mode (run_20260425_210107_508_0).
    #[test]
    fn parse_recovers_inner_action_extracted_by_strip_fences() {
        let raw = "thought: explain step\naction_input: {\"action\":\"click\",\"target\":\"#submit\"}\ndone: false";
        let s = parse_step(raw);
        assert!(s.parse_error.is_none(), "got parse_error: {:?}", s.parse_error);
        assert_eq!(s.action_input["action"], "click");
    }

    /// Plaintext fallback: when the entire raw response is unparseable as JSON
    /// (e.g. multiple top-level keys without quotes), parse_plaintext_step
    /// extracts the action_input block via brace-depth counting.
    #[test]
    fn parse_plaintext_fallback_extracts_action_input() {
        let raw = "thought: 我已經完成步驟 1，現在執行步驟 2\n\
                   action_input: {\"action\":\"shadow_click\",\"role\":\"tab\",\"name_regex\":\"^商品$\"}\n\
                   done: false";
        let s = parse_step(raw);
        assert!(s.parse_error.is_none(), "got parse_error: {:?}", s.parse_error);
        assert_eq!(s.action_input["action"], "shadow_click");
        assert_eq!(s.action_input["name_regex"], "^商品$");
        assert!(!s.done);
    }

    /// Plaintext fallback respects nested braces — the matching `}` must be at
    /// the right depth (not the first one encountered inside a string).
    #[test]
    fn parse_plaintext_fallback_handles_nested_braces() {
        let raw = "action_input: {\"action\":\"x\",\"meta\":{\"foo\":\"bar\"}}";
        let s = parse_step(raw);
        assert!(s.parse_error.is_none());
        assert_eq!(s.action_input["meta"]["foo"], "bar");
    }

    /// Plaintext fallback returns parse_error when neither shape matches —
    /// e.g. just prose with no extractable JSON.
    #[test]
    fn parse_plaintext_fallback_returns_error_when_no_action() {
        let raw = "I'm thinking about what to do next, but cannot decide.";
        let s = parse_step(raw);
        assert!(s.parse_error.is_some());
    }

    // ── slugify_for_topic ───────────────────────────────────────────────────

    #[test]
    fn slugify_replaces_non_alnum_with_hyphens_and_dedups() {
        assert_eq!(slugify_for_topic("shadow_click:tab:^商品$", 40), "shadow-click-tab");
        // CJK is not ASCII-alphanumeric so it's stripped — that's by design;
        // topicKeys must be ASCII-safe for KB MCP.
        assert_eq!(slugify_for_topic("foo / bar // baz", 40), "foo-bar-baz");
    }

    #[test]
    fn slugify_preserves_existing_hyphens_and_underscores() {
        assert_eq!(slugify_for_topic("agora_pickup_checkboxes_restore", 40),
                   "agora-pickup-checkboxes-restore");
        assert_eq!(slugify_for_topic("test-id-with-dashes", 40),
                   "test-id-with-dashes");
    }

    #[test]
    fn slugify_lowercases_uppercase() {
        assert_eq!(slugify_for_topic("Agora_Market_Smoke", 40),
                   "agora-market-smoke");
    }

    #[test]
    fn slugify_truncates_to_max_len() {
        let long = "a".repeat(200);
        assert!(slugify_for_topic(&long, 40).len() <= 40);
    }

    #[test]
    fn slugify_strips_leading_and_trailing_separators() {
        assert_eq!(slugify_for_topic("---foo---", 40), "foo");
        assert_eq!(slugify_for_topic("///bar///", 40), "bar");
    }

    /// Plaintext fallback honours `done: true` even without a JSON wrapper.
    #[test]
    fn parse_plaintext_fallback_detects_done_true() {
        let raw = "action_input: {\"action\":\"goto\",\"target\":\"https://x.com\"}\ndone: true";
        let s = parse_step(raw);
        assert!(s.parse_error.is_none());
        assert!(s.done);
    }

    // ── Convergence-guard signature tests ───────────────────────────────────

    #[test]
    fn signature_uses_action_and_target() {
        let a = json!({"action":"goto","target":"https://x.com"});
        let b = json!({"action":"goto","target":"https://x.com"});
        let c = json!({"action":"goto","target":"https://y.com"});
        assert_eq!(action_signature(&a), action_signature(&b));
        assert_ne!(action_signature(&a), action_signature(&c));
    }

    #[test]
    fn signature_includes_role_for_shadow_actions() {
        let a = json!({"action":"shadow_click","role":"tab","name_regex":"^商品$"});
        let b = json!({"action":"shadow_click","role":"tab","name_regex":"^訂單$"});
        let c = json!({"action":"shadow_click","role":"tab","name_regex":"^商品$"});
        assert_ne!(
            action_signature(&a),
            action_signature(&b),
            "different name_regex should yield different signature"
        );
        assert_eq!(action_signature(&a), action_signature(&c));
    }

    #[test]
    fn signature_includes_backend_id_when_no_target_or_role() {
        let a = json!({"action":"ax_click","backend_id":42});
        let b = json!({"action":"ax_click","backend_id":99});
        assert_ne!(action_signature(&a), action_signature(&b));
    }

    #[test]
    fn signature_truncates_long_text_to_keep_compact() {
        let long = "a".repeat(200);
        let a = json!({"action":"flutter_type","text": long});
        let sig = action_signature(&a);
        // Truncated to 24 chars + ellipsis — keeps signatures bounded.
        assert!(sig.len() < 60, "signature too long: {sig} ({})", sig.len());
        assert!(sig.contains("…"), "expected truncation ellipsis, got {sig}");
    }

    #[test]
    fn signature_distinguishes_action_families() {
        let click = json!({"action":"click","target":"#submit"});
        let goto  = json!({"action":"goto","target":"#submit"});
        assert_ne!(action_signature(&click), action_signature(&goto));
    }

    // ── is_noise_action / is_error_observation ──────────────────────────────

    #[test]
    fn noise_actions_recognized() {
        for a in ["wait", "sleep", "enable_a11y", "screenshot", "page_state", "url", "title"] {
            assert!(
                is_noise_action(&json!({"action": a})),
                "expected {a} to be noise"
            );
        }
    }

    #[test]
    fn substantive_actions_are_not_noise() {
        for a in [
            "click", "shadow_click", "ax_click", "type", "ax_type", "goto",
            "screenshot_analyze", "go_back",
        ] {
            assert!(
                !is_noise_action(&json!({"action": a})),
                "expected {a} to be substantive"
            );
        }
    }

    #[test]
    fn noise_action_check_is_case_insensitive() {
        assert!(is_noise_action(&json!({"action":"WAIT"})));
        assert!(is_noise_action(&json!({"action":"Enable_A11Y"})));
    }

    #[test]
    fn error_observation_detects_error_prefix() {
        assert!(is_error_observation("ERROR: shadow_click: host empty"));
        assert!(is_error_observation("  ERROR: foo")); // leading whitespace
        assert!(is_error_observation("Err(some failure)"));
    }

    #[test]
    fn error_observation_detects_json_error_field() {
        assert!(is_error_observation(r#"{"error":"thing failed"}"#));
        assert!(is_error_observation(r#"{"status":"failed"}"#));
        assert!(is_error_observation(r#"{"status":"timeout","ms":1000}"#));
    }

    #[test]
    fn error_observation_returns_false_for_success() {
        assert!(!is_error_observation(r#"{"status":"clicked"}"#));
        assert!(!is_error_observation(r#"{"ms":3000,"status":"slept"}"#));
        assert!(!is_error_observation(r#"{"ax_node_count":39}"#));
        assert!(!is_error_observation(""));
        assert!(!is_error_observation("Some unrelated text"));
    }

    // ── Issue #103: dispute_yaml ─────────────────────────────────────────────

    /// `parse_step` must accept a `dispute_yaml` action without parse error.
    /// The action has three optional fields in addition to "action".
    #[test]
    fn dispute_yaml_parses_correctly() {
        let raw = r#"{
            "thought": "Step 2 already on target page — spec is wrong",
            "action_input": {
                "action": "dispute_yaml",
                "reason": "step 2 商品 tab 已 selected，click 沒反應",
                "suspected_step": 2,
                "suggested_fix": "改成 conditional: if not on page, click"
            },
            "done": false
        }"#;
        let s = parse_step(raw);
        assert!(s.parse_error.is_none(), "dispute_yaml should parse cleanly: {:?}", s.parse_error);
        assert_eq!(s.action_input["action"], "dispute_yaml");
        assert_eq!(s.action_input["suspected_step"], 2);
        assert!(!s.done, "dispute_yaml should not set done=true via parse_step");
    }

    /// `dispute_yaml` without optional fields (reason only) must also parse.
    #[test]
    fn dispute_yaml_parses_reason_only() {
        let raw = r#"{"thought":"x","action_input":{"action":"dispute_yaml","reason":"element missing"},"done":false}"#;
        let s = parse_step(raw);
        assert!(s.parse_error.is_none());
        assert_eq!(s.action_input["action"], "dispute_yaml");
        // suspected_step absent → Value::Null
        assert!(s.action_input.get("suspected_step").map_or(true, |v| v.is_null()));
    }

    /// `DisputeInfo` deserialized from `action_input` must carry the correct fields.
    #[test]
    fn dispute_info_extracts_fields_correctly() {
        let action_input = json!({
            "action": "dispute_yaml",
            "reason": "step 3 already on destination",
            "suspected_step": 3,
            "suggested_fix": "remove step 3 or add condition"
        });
        let reason = action_input.get("reason").and_then(Value::as_str)
            .unwrap_or("").to_string();
        let suspected_step = action_input.get("suspected_step").and_then(Value::as_i64);
        let suggested_fix = action_input.get("suggested_fix").and_then(Value::as_str)
            .map(String::from);
        let di = DisputeInfo { reason: reason.clone(), suspected_step, suggested_fix };

        assert_eq!(di.reason, "step 3 already on destination");
        assert_eq!(di.suspected_step, Some(3));
        assert_eq!(di.suggested_fix.as_deref(), Some("remove step 3 or add condition"));
    }

    /// `handle_dispute_yaml` returns a `DisputeInfo` with the reason intact
    /// and does NOT panic when KB and gh CLI are unavailable.
    ///
    /// KB write is async fire-and-forget and KB_ENABLED defaults to false in
    /// tests.  gh CLI invocation may fail (not installed in CI) — the function
    /// must log a warning and continue rather than panicking.
    #[test]
    fn dispute_yaml_handle_is_non_panicking() {
        // Tokio runtime needed for the fire-and-forget KB spawn inside.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let info = DisputeInfo {
                reason: "unit-test dispute reason".into(),
                suspected_step: Some(1),
                suggested_fix: Some("test fix".into()),
            };
            // No run_id — tests that gh CLI may be missing; should not panic.
            let out = handle_dispute_yaml(
                "test_dispute_non_panic",
                Some("run_test_123"),
                0,
                info,
                "https://example.com",
                "gemini-test",
            );
            assert_eq!(out.reason, "unit-test dispute reason");
            assert_eq!(out.suspected_step, Some(1));
        });
    }

    /// `TestStatus::Disputed` round-trips through serde_json.
    #[test]
    fn test_status_disputed_serializes_as_lowercase() {
        let s = serde_json::to_string(&TestStatus::Disputed).unwrap();
        assert_eq!(s, r#""disputed""#);
        let back: TestStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(back, TestStatus::Disputed);
    }
}
