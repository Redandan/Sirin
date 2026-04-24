//! ReAct-style test executor — LLM drives browser actions to achieve a goal.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::parser::TestGoal;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestStep {
    pub thought: String,
    pub action: Value,        // {"action":"click","target":"#btn"}
    pub observation: String,  // truncated tool result or ERROR:...
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus { Passed, Failed, Timeout, Error }

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

    if let Some(rid) = run_id {
        runs::set_phase(rid, runs::RunPhase::Running { step: 0, current_action: "goto".into() });
    }

    // 0-pre) Warn if the test declares required reading that the caller must
    // have done before this run.  Surfaced here so it appears in Sirin logs
    // even for runs started without going through the MCP layer.
    if !test.docs_refs.is_empty() {
        tracing::warn!(
            "[test_runner] ⚠️  '{}' has {} required doc(s) — confirm read before interpreting results:\n{}",
            test.id,
            test.docs_refs.len(),
            test.docs_refs.iter().map(|d| format!("  • {d}")).collect::<Vec<_>>().join("\n")
        );
    }

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
    {
        let mut wait_input = json!({"action": "wait", "timeout_ms": 8000});
        inject_session(&mut wait_input, session_id);
        let _ = ctx.call_tool("web_navigate", wait_input).await;

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

    // 2b) Run fixture setup steps (failure aborts the test before the ReAct loop).
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
                };
            }

            let hint_for_llm = parse_error_hint.take(); // reset — only used for the next turn

            // Perception capture — zero-overhead for PerceptionMode::Text (short-
            // circuits inside perceive).  Done once per iteration and reused across
            // the 3 LLM retry attempts below so we don't re-screenshot on transient
            // backend errors.
            let perception = crate::perception::perceive(ctx, test.perception).await;

            // LLM call with retry for transient network errors (e.g. "error decoding
            // response body" from Gemini when Chrome crashes cause concurrent request
            // interference).  We retry up to 3× with a short back-off before giving up.
            let raw = {
                let mut last_err = String::new();
                let mut raw_opt: Option<String> = None;
                for attempt in 0u32..3 {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    match call_test_llm(ctx, test, &history, hint_for_llm.as_deref(), &perception).await {
                        Ok(s) => { raw_opt = Some(s); break; }
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
                    Some(s) => s,
                    None => break 'react finalize_early(
                        ctx, run_id, test, &history,
                        format!("LLM error after 3 attempts: {last_err}")
                    ).await,
                }
            };

            let step = parse_step(&raw);
            if let Some(err) = &step.parse_error {
                parse_error_count += 1;
                if parse_error_count >= max_parse_errors {
                    history.push(TestStep {
                        thought: step.thought.clone(),
                        action: json!({"error": "invalid_json"}),
                        observation: format!("ERROR: {err}"),
                    });
                    if let Some(rid) = run_id {
                        runs::push_observation(rid, format!("ERROR (parse): {err}\nRaw: {raw}"));
                    }
                    break 'react finalize_early(
                        ctx, run_id, test, &history,
                        format!("too many invalid LLM responses ({max_parse_errors})"),
                    ).await;
                }
                // Reprompt — save hint for next iteration
                parse_error_hint = Some(format!(
                    "⚠️ Previous response could not be parsed as JSON ({err}). \
                     Please output STRICTLY valid JSON, no markdown fences, no prose before/after."
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
                        history.push(TestStep {
                            thought: step.thought,
                            action: action_input,
                            observation: recovery_obs,
                        });
                        continue;  // next iteration — LLM will see recovery message
                    }
                }
            }

            // Store full observation before truncation
            if let Some(rid) = run_id {
                runs::push_observation(rid, full_obs.clone());
            }
            let obs_for_llm = truncate_with_hint(&full_obs, history.len());

            history.push(TestStep {
                thought: step.thought,
                action: action_input,
                observation: obs_for_llm,
            });
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

    format!(
        r##"You are a browser-testing agent driving a live Chrome window.  The attached image is a PNG screenshot of the **current viewport** — treat it as the primary observation.

## Goal
{goal}

## Page context
{summary}

## Success criteria
{criteria}

## Preferred actions (canvas-safe; call via tool "web_navigate", field "action")
- click_point    — x, y: viewport pixel coords (from the screenshot).  Use this as the default for Flutter / canvas apps.
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
  "action_input": {{ "action": "click_point", "x": 640, "y": 420 }},
  "done": false,
  "final_answer": "<only when done=true: summary of outcome in {lang}>"
}}
"##,
        goal = test.goal.trim(),
        summary = perception.summary,
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

    format!(r##"You are a browser-testing agent.  Your job is to achieve the test goal by driving the browser.

## Goal
{goal}

## Test URL (already opened)
{url}

## Success criteria
{criteria}

## Available browser actions (call via tool "web_navigate", field "action")
- goto           — target: URL
- screenshot     — capture page PNG
- click          — target: CSS selector OR plain text label (e.g. "使用用戶名密碼登入"); plain text triggers XPath text search
- type           — target: CSS selector, text: input text
- read           — target: CSS selector → returns innerText
- eval           — target: JS expression → returns result
- wait           — target: CSS selector (waits for element) OR plain ms number (sleeps, e.g. "2000")
- exists         — target: CSS selector → true/false
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

Fallback to ax_find/ax_click if shadow_find returns "no shadow root".

When you need EXACT text comparison (numbers, IDs), prefer ax_* over
screenshot_analyze (which approximates).

## Robustness actions (test isolation + race-free)
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
async fn call_test_llm(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
    history: &[TestStep],
    parse_error_hint: Option<&str>,
    perception: &crate::perception::PagePerception,
) -> Result<String, String> {
    // Vision path: attach screenshot as primary observation.  Still requires
    // the resolved mode to be Vision AND the capture to have succeeded; if
    // the screenshot failed (None), we gracefully fall back to text prompt.
    if matches!(
        perception.resolved_mode,
        crate::perception::PerceptionMode::Vision
    ) {
        if let Some(b64) = perception.screenshot_b64.as_deref() {
            let prompt = build_prompt_vision(test, history, parse_error_hint, perception);
            return crate::llm::call_vision(
                ctx.http.as_ref(),
                ctx.llm.as_ref(),
                &prompt,
                b64,
                "image/png",
            )
            .await
            .map_err(|e| e.to_string());
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
            call_claude_cli(prompt).await
        }
        _ => {
            let prompt = build_prompt(test, history, parse_error_hint);
            crate::llm::call_coding_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt)
                .await
                .map_err(|e| e.to_string())
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
            let action_input = v.get("action_input").cloned().unwrap_or(json!({}));
            let done = v.get("done").and_then(Value::as_bool).unwrap_or(false);
            let final_answer = v.get("final_answer").and_then(Value::as_str).map(String::from).filter(|s| !s.is_empty());

            // Require action_input to include an "action" field unless done
            if !done && action_input.get("action").and_then(Value::as_str).is_none() {
                return ParsedStep {
                    thought, action_input, done, final_answer,
                    parse_error: Some("action_input missing 'action' field".into()),
                };
            }
            ParsedStep { thought, action_input, done, final_answer, parse_error: None }
        }
        Err(e) => ParsedStep {
            parse_error: Some(format!("JSON parse: {e}")),
            ..Default::default()
        },
    }
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
}
