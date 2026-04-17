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
const OBS_TRUNCATE_CHARS: usize = 800;

/// Run a single fixture step via the `web_navigate` tool.
async fn run_fixture_step(
    ctx: &crate::adk::context::AgentContext,
    step: &crate::test_runner::parser::FixtureStep,
) -> Result<(), String> {
    let mut args = json!({
        "action": step.action,
        "target": step.target,
        "text": step.text,
    });
    if let Some(ms) = step.timeout_ms {
        args["timeout"] = json!(ms);
    }
    ctx.call_tool("web_navigate", args).await
        .map(|_| ())
        .map_err(|e| format!("fixture step '{}' failed: {}", step.action, e))
}

/// Execute a test goal by driving the browser via the `web_navigate` tool.
pub async fn execute_test(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
) -> TestResult {
    execute_test_tracked(ctx, test, None).await
}

/// Same as [`execute_test`] but reports live progress to an async run registry.
/// `run_id` — key in [`crate::test_runner::runs`] to update as steps progress.
pub async fn execute_test_tracked(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
    run_id: Option<&str>,
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

    // 0) Ensure browser launched in the right headless mode.
    // Flutter CanvasKit/WebGL needs headless=false to actually paint.
    let want_headless = test.browser_headless.unwrap_or_else(crate::browser::default_headless);
    if let Err(e) = tokio::task::spawn_blocking(move || crate::browser::ensure_open(want_headless))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))
        .and_then(|r| r)
    {
        return finalize_early(ctx, run_id, test, &history, format!("browser launch failed: {e}")).await;
    }

    // 1) Navigate to the test URL (with url_query params merged in).
    let nav_url = test.full_url();
    let nav_input = json!({ "action": "goto", "target": &nav_url });
    if let Err(e) = ctx.call_tool("web_navigate", nav_input).await {
        return finalize_early(ctx, run_id, test, &history, format!("navigate failed: {e}")).await;
    }

    // 2) Install console + network capture (best effort)
    let _ = ctx.call_tool("web_navigate", json!({ "action": "install_capture" })).await;

    // 2b) Run fixture setup steps (failure aborts the test before the ReAct loop).
    if let Some(fixture) = &test.fixture {
        for step in &fixture.setup {
            if let Err(e) = run_fixture_step(ctx, step).await {
                let result = finalize_early(ctx, run_id, test, &history, format!("fixture setup failed: {e}")).await;
                // Still run cleanup even when setup fails.
                if let Some(fix) = &test.fixture {
                    for cs in &fix.cleanup {
                        if let Err(ce) = run_fixture_step(ctx, cs).await {
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

            let prompt = build_prompt(test, &history, parse_error_hint.as_deref());
            parse_error_hint = None;  // reset — only used for the next turn

            let raw = match crate::llm::call_coding_prompt(
                ctx.http.as_ref(),
                ctx.llm.as_ref(),
                prompt,
            ).await {
                Ok(s) => s,
                Err(e) => break 'react finalize_early(ctx, run_id, test, &history, format!("LLM error: {e}")).await,
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
            let action_input = step.action_input.clone();
            let action_label = action_input.get("action").and_then(Value::as_str).unwrap_or("?").to_string();
            if let Some(rid) = run_id {
                runs::set_phase(rid, runs::RunPhase::Running {
                    step: (iteration + 1) as u32,
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
            if let Err(e) = run_fixture_step(ctx, step).await {
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
        || crate::browser::screenshot()
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

fn build_prompt(test: &TestGoal, history: &[TestStep], parse_error_hint: Option<&str>) -> String {
    let history_str = if history.is_empty() {
        "(none yet)".to_string()
    } else {
        history.iter().enumerate().map(|(i, s)| {
            let obs = truncate(&s.observation, 500);
            format!("[Step {}]\nThought: {}\nAction: {}\nObservation: {}\n",
                i + 1, s.thought, s.action, obs)
        }).collect::<Vec<_>>().join("---\n")
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
- click          — target: CSS selector
- type           — target: CSS selector, text: input text
- read           — target: CSS selector → returns innerText
- eval           — target: JS expression → returns result
- wait           — target: CSS selector, timeout: ms
- exists         — target: CSS selector → true/false
- attr           — target: selector, text: attribute name
- scroll         — x, y: pixels (default 0, 300)
- scroll_to      — target: selector
- key            — target: key name (Enter/Tab/Escape)
- screenshot_analyze — target: question for vision LLM about the page
- console        — return captured console messages
- network        — return captured fetch/XHR

## Accessibility tree actions (literal text, no vision approximation)
For exact-string assertions ($7376.80, error messages, token counts):
- enable_a11y       — trigger Flutter semantics bridge first (Canvas apps)
- ax_tree           — list all a11y nodes (role + literal name + value + backend_id)
- ax_find           — role and/or name (substring); returns single match
- ax_value          — backend_id → exact text (value || name)
- ax_click          — backend_id → click via DOM box model centre
- ax_focus          — backend_id → DOM focus
- ax_type           — backend_id, text → focus + insertText
- ax_type_verified  — same as ax_type + read-back; returns {{typed, actual, matched}}

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
