//! ReAct iteration phase — the think→act→observe loop.
//!
//! On each iteration the LLM receives a trimmed history window, chooses
//! either a tool call or `DONE`, and its output is appended to
//! [`RunState::history`].  File-read results are pinned + memoised so the
//! model can't waste iterations re-reading the same file.
//!
//! Fail-fast triggers that abort the loop early:
//! - `MAX_TOTAL_TOOL_ERRORS` (3) accumulated errors
//! - `max_stalled_iterations` consecutive no-progress iterations
//! - `MAX_PATCH_ERRORS` (2) consecutive `file_patch` failures block all
//!   write tools (prevents escalation to destructive `file_write`)

use serde_json::{json, Value};

use crate::adk::AgentContext;
use crate::persona::CodingAgentConfig;
use crate::sirin_log;

use super::helpers::{describe_tools, file_read_cache_key, preview_text, step_fingerprint};
use super::prompt::{build_react_prompt, parse_react_step};
use super::state::RunState;
use super::verdict::{
    build_fail_fast_outcome, has_sufficient_analysis_evidence, salvage_non_json_final_answer,
    synthesize_read_only_outcome, HistoryEntry,
};
use super::CodingRequest;

/// Sliding window sizes — keep the prompt compact so the main model doesn't
/// choke on a 20k-char history on long tasks.
/// Reduced from 6 to 4 to decrease token consumption while maintaining sufficient context.
const HISTORY_WINDOW: usize = 4;
const MAX_PINNED: usize = 4;

const MAX_PATCH_ERRORS: u32 = 2;
const MAX_TOTAL_TOOL_ERRORS: u32 = 3;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_react_iterations(
    ctx: &AgentContext,
    request: &CodingRequest,
    _config: &CodingAgentConfig,
    project_ctx: &str,
    plan: &str,
    dry_run: bool,
    read_only_analysis: bool,
    max_iter: usize,
    state: &mut RunState,
) -> Result<(), String> {
    let mut total_tool_errors: u32 = 0;
    let mut stalled_iterations: u32 = 0;
    let mut last_step_fingerprint: Option<String> = None;
    let tool_list = describe_tools();

    let mut consecutive_patch_errors: u32 = 0;
    let max_stalled_iterations: u32 = if read_only_analysis { 2 } else { 3 };
    let write_tools = ["file_write", "file_patch", "plan_execute"];

    // Read cache: deduplicates local_file_read calls within one task so the
    // model can't waste iterations re-reading files it already inspected.
    // Key = file path.  Value = (first_read_iteration, formatted_content).
    // Invalidated when file_patch / file_write succeeds on that path.
    let mut file_read_cache: std::collections::HashMap<String, (usize, String)> =
        std::collections::HashMap::new();

    for iteration in 0..max_iter {
        // Build history window: capped pinned entries + recent N non-pinned.
        let history_window: Vec<&HistoryEntry> = {
            let mut pinned: Vec<&HistoryEntry> =
                state.history.iter().filter(|h| h.pinned).collect();
            if pinned.len() > MAX_PINNED {
                pinned = pinned[pinned.len() - MAX_PINNED..].to_vec();
            }
            let recent: Vec<&HistoryEntry> = state
                .history
                .iter()
                .filter(|h| !h.pinned)
                .rev()
                .take(HISTORY_WINDOW)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            pinned.into_iter().chain(recent).collect()
        };

        let prompt = build_react_prompt(
            &request.task,
            project_ctx,
            plan,
            &history_window,
            &tool_list,
            dry_run,
        );

        // #263 Phase 2 — go through the ctx.llm_caller trait so tests can
        // inject MockLlmCaller with a deterministic response queue.
        let raw = match ctx.llm_caller.call_coding(prompt).await {
            Ok(raw) => raw,
            Err(e) => {
                let err_msg = format!("LLM error on iteration {iteration}: {e}");
                if read_only_analysis && has_sufficient_analysis_evidence(&state.history) {
                    state.had_tool_errors = true;
                    state.last_tool_error = Some(preview_text(&err_msg));
                    state.final_answer = format!(
                        "⚠️ {err_msg}\n\n{}",
                        synthesize_read_only_outcome(&state.history)
                    );
                    break;
                }
                return Err(err_msg);
            }
        };

        let step = parse_react_step(&raw);
        sirin_log!(
            "[coding_agent] iter={} action={} thought={}",
            iteration,
            step.action,
            preview_text(&step.thought)
        );

        ctx.record_system_event(
            format!("adk_coding_iter_{iteration}"),
            Some(format!("action={}", step.action)),
            Some("RUNNING"),
            Some(preview_text(&step.thought)),
        );

        if step.parse_error {
            if read_only_analysis && has_sufficient_analysis_evidence(&state.history) {
                state.final_answer = salvage_non_json_final_answer(&raw, &state.history);
                state.history.push(HistoryEntry {
                    thought: step.thought,
                    action: "DONE".to_string(),
                    action_input: json!({}),
                    observation: "Task complete (salvaged non-JSON analysis answer).".to_string(),
                    pinned: false,
                });
                break;
            }

            let observation = format!(
                "ERROR: LLM step was not valid JSON. Retry with ONLY the required JSON object and do not mark DONE until you have either verified the existing code or applied a real change. Raw preview: {}",
                preview_text(&raw),
            );
            state.history.push(HistoryEntry {
                thought: step.thought,
                action: "INVALID_JSON".to_string(),
                action_input: json!({}),
                observation: observation.clone(),
                pinned: false,
            });
            state.had_tool_errors = true;
            total_tool_errors += 1;
            state.last_tool_error = Some(preview_text(&observation));

            if total_tool_errors >= MAX_TOTAL_TOOL_ERRORS {
                state.final_answer = build_fail_fast_outcome(
                    "模型連續回傳不可解析的輸出，已觸發 fail-fast",
                    &state.history,
                    state.last_tool_error.as_deref(),
                    read_only_analysis,
                );
                ctx.record_system_event(
                    "adk_coding_fail_fast",
                    Some("⚠ fail-fast：模型連續回傳無效 JSON".to_string()),
                    Some("FOLLOWUP_NEEDED"),
                    state.last_tool_error.clone(),
                );
                break;
            }
            continue;
        }

        if step.action == "DONE" {
            state.final_answer = step.final_answer.unwrap_or_else(|| step.thought.clone());
            state.history.push(HistoryEntry {
                thought: step.thought,
                action: "DONE".to_string(),
                action_input: json!({}),
                observation: "Task complete.".to_string(),
                pinned: false,
            });
            break;
        }

        // Safety: if file_patch has failed too many times, block all write tools
        // to prevent the LLM from escalating to file_write as a fallback.
        if consecutive_patch_errors >= MAX_PATCH_ERRORS
            && write_tools.contains(&step.action.as_str())
        {
            sirin_log!(
                "[coding_agent] SAFETY: write tool '{}' blocked after {} consecutive patch errors",
                step.action,
                consecutive_patch_errors
            );
            state.final_answer = format!(
                "任務中止：file_patch 連續失敗 {} 次，已封鎖所有寫入工具以防止資料損毀。\
                請縮小任務範圍，並確認目標函式的確切位置後重試。",
                consecutive_patch_errors
            );
            break;
        }

        // Execute the tool.
        // In dry_run mode, force dry_run=true into every write tool so that
        // writes are never applied regardless of how the agent calls them.
        let is_write_tool = matches!(
            step.action.as_str(),
            "file_write" | "file_patch" | "plan_execute"
        );
        if is_write_tool {
            state.attempted_write = true;
        }
        let tool_input = if dry_run && is_write_tool {
            let mut input = step.action_input.clone();
            if let Some(obj) = input.as_object_mut() {
                obj.insert("dry_run".to_string(), json!(true));
            }
            input
        } else {
            step.action_input.clone()
        };

        let is_file_read = step.action == "local_file_read";

        // Invalidate cache when a file is about to be modified so a subsequent
        // read fetches the updated content instead of the stale cached version.
        if step.action == "file_patch" || step.action == "file_write" {
            if let Some(path) = step.action_input.get("path").and_then(Value::as_str) {
                let path = path.to_string();
                file_read_cache
                    .retain(|key, _| !key.starts_with(&format!("{path}|")) && key != &path);
            }
        }

        // Short-circuit duplicate file reads: return cached content immediately
        // without consuming an API round-trip.
        let observation = if is_file_read {
            let path_key = file_read_cache_key(&step.action_input);
            if let Some((cached_iter, cached)) = file_read_cache.get(&path_key) {
                sirin_log!(
                    "[coding_agent] cache hit: {path_key} (first read at iter {cached_iter})"
                );
                format!("[Already read at iteration {cached_iter} — content unchanged, using cache]\n{cached}")
            } else {
                let outcome = super::step::execute_tool(ctx, &step.action, tool_input, true).await;
                if !path_key.is_empty() && !outcome.observation.starts_with("ERROR:") {
                    file_read_cache.insert(path_key, (iteration, outcome.observation.clone()));
                }
                outcome.observation
            }
        } else {
            let outcome = super::step::execute_tool(ctx, &step.action, tool_input, false).await;
            for path in outcome.files_modified {
                if !state.files_modified.contains(&path) {
                    state.files_modified.push(path);
                }
            }
            outcome.observation
        };

        let action_name = step.action.clone();
        state.history.push(HistoryEntry {
            thought: step.thought,
            action: step.action,
            action_input: step.action_input,
            observation: observation.clone(),
            pinned: is_file_read,
        });

        // Track consecutive patch errors for safety circuit breaker.
        if observation.starts_with("ERROR:") {
            state.had_tool_errors = true;
            total_tool_errors += 1;
            state.last_tool_error = Some(preview_text(&observation));
            sirin_log!("[coding_agent] tool error: {observation}");
            if action_name == "file_patch" {
                consecutive_patch_errors += 1;
            }
        } else if action_name == "file_patch" {
            consecutive_patch_errors = 0; // reset on success
        }

        let fingerprint = step_fingerprint(
            &action_name,
            &state
                .history
                .last()
                .expect("history entry just pushed")
                .action_input,
            &observation,
        );
        let repeated_cache_hit = action_name == "local_file_read"
            && observation.starts_with("[Already read at iteration");
        if repeated_cache_hit || last_step_fingerprint.as_deref() == Some(&fingerprint) {
            stalled_iterations += 1;
        } else {
            stalled_iterations = 0;
        }
        last_step_fingerprint = Some(fingerprint);

        if read_only_analysis
            && repeated_cache_hit
            && has_sufficient_analysis_evidence(&state.history)
        {
            state.final_answer = synthesize_read_only_outcome(&state.history);
            ctx.record_system_event(
                "adk_coding_fail_fast",
                Some("✅ read-only analysis：grounded answer ready".to_string()),
                Some("DONE"),
                Some("Stopped after repeated cached reads because enough grounded evidence was already collected.".to_string()),
            );
            break;
        }

        if total_tool_errors >= MAX_TOTAL_TOOL_ERRORS {
            state.final_answer = build_fail_fast_outcome(
                "工具錯誤次數過多，已觸發 fail-fast",
                &state.history,
                state.last_tool_error.as_deref(),
                read_only_analysis,
            );
            ctx.record_system_event(
                "adk_coding_fail_fast",
                Some("⚠ fail-fast：工具錯誤次數過多".to_string()),
                Some("FOLLOWUP_NEEDED"),
                state.last_tool_error.clone(),
            );
            break;
        }

        if stalled_iterations >= max_stalled_iterations {
            state.final_answer = build_fail_fast_outcome(
                "連續多步沒有新進展，已觸發 fail-fast",
                &state.history,
                state.last_tool_error.as_deref(),
                read_only_analysis,
            );
            ctx.record_system_event(
                "adk_coding_fail_fast",
                Some("⚠ fail-fast：連續多步沒有新進展".to_string()),
                Some("FOLLOWUP_NEEDED"),
                Some(format!("stalled_iterations={stalled_iterations}")),
            );
            break;
        }
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// Issue #263 Phase 2 — exercise the ReAct loop with `MockLlmCaller` so we can
// drive deterministic LLM responses without spinning up a real backend.  The
// mock is wired in via `AgentContext::with_llm_caller`; tests here cover the
// loop's terminating branches (DONE / parse-error fail-fast / max-iter exit)
// without touching any tools.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::adk::tool::ToolRegistry;
    use crate::adk::AgentContext;
    use crate::llm::{LlmKind, MockLlmCaller};
    use crate::persona::CodingAgentConfig;

    /// Build a minimal `AgentContext` with the given mock caller wired in.
    /// Empty tool registry — tests in this file shouldn't drive tools, only
    /// terminate via DONE / fail-fast / max_iter.
    fn ctx_with_mock(mock: Arc<MockLlmCaller>) -> AgentContext {
        AgentContext::new("test-react", ToolRegistry::new()).with_llm_caller(mock)
    }

    fn empty_request() -> CodingRequest {
        CodingRequest {
            task: "trivial test task".to_string(),
            max_iterations: Some(1),
            dry_run: false,
            context_block: None,
        }
    }

    #[tokio::test]
    async fn react_loop_terminates_on_done_action() {
        let mock = Arc::new(MockLlmCaller::with_responses(vec![Ok(
            r#"{"thought":"task is trivially done","action":"DONE","final_answer":"hello world"}"#
                .to_string(),
        )]));
        let ctx = ctx_with_mock(Arc::clone(&mock));
        let request = empty_request();
        let config = CodingAgentConfig::default();
        let mut state = RunState::default();

        let result = run_react_iterations(
            &ctx,
            &request,
            &config,
            "project context",
            "plan",
            false,
            false,
            5,
            &mut state,
        )
        .await;

        assert!(result.is_ok(), "loop should succeed: {result:?}");
        assert_eq!(state.final_answer, "hello world");
        assert_eq!(state.history.len(), 1, "exactly one iteration recorded");
        assert_eq!(state.history[0].action, "DONE");
        assert!(!state.had_tool_errors);

        // Mock invariants — only one call, on the coding-tier endpoint.
        let captured = mock.captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, LlmKind::Coding);
        // The prompt must include the task and the project context block.
        let prompt = &captured[0].1;
        assert!(
            prompt.contains("trivial test task"),
            "prompt must include task"
        );
        assert!(
            prompt.contains("project context"),
            "prompt must include project ctx"
        );
    }

    #[tokio::test]
    async fn react_loop_falls_back_to_thought_when_final_answer_missing() {
        let mock = Arc::new(MockLlmCaller::with_responses(vec![Ok(
            r#"{"thought":"answer-from-thought","action":"DONE"}"#.to_string(),
        )]));
        let ctx = ctx_with_mock(Arc::clone(&mock));
        let request = empty_request();
        let config = CodingAgentConfig::default();
        let mut state = RunState::default();

        run_react_iterations(
            &ctx, &request, &config, "ctx", "plan", false, false, 5, &mut state,
        )
        .await
        .unwrap();

        // When `final_answer` is absent, the loop falls back to the thought
        // text (see line 177 in this file).  Pinning that contract here so a
        // future refactor doesn't silently drop it.
        assert_eq!(state.final_answer, "answer-from-thought");
    }

    #[tokio::test]
    async fn react_loop_fails_fast_after_three_invalid_json_responses() {
        // Three consecutive non-JSON outputs trip MAX_TOTAL_TOOL_ERRORS.
        let mock = Arc::new(MockLlmCaller::with_responses(vec![
            Ok("not json at all".to_string()),
            Ok("still garbage".to_string()),
            Ok("third strike".to_string()),
        ]));
        let ctx = ctx_with_mock(Arc::clone(&mock));
        let request = empty_request();
        let config = CodingAgentConfig::default();
        let mut state = RunState::default();

        let result = run_react_iterations(
            &ctx, &request, &config, "ctx", "plan", false, false, 10, &mut state,
        )
        .await;

        assert!(result.is_ok(), "fail-fast is a clean break, not an Err");
        assert!(state.had_tool_errors, "errors must be recorded");
        assert!(
            state.final_answer.contains("fail-fast") || state.final_answer.contains("無效"),
            "final_answer should mention fail-fast: {}",
            state.final_answer
        );
        // Exactly 3 LLM calls — one per parse error before the threshold trips.
        assert_eq!(mock.call_count(), 3);
    }

    #[tokio::test]
    async fn react_loop_propagates_llm_error() {
        let mock = Arc::new(MockLlmCaller::with_responses(vec![Err(
            "simulated 429 rate-limit".to_string(),
        )]));
        let ctx = ctx_with_mock(Arc::clone(&mock));
        let request = empty_request();
        let config = CodingAgentConfig::default();
        let mut state = RunState::default();

        let result = run_react_iterations(
            &ctx, &request, &config, "ctx", "plan", false, false, 5, &mut state,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("LLM error on iteration 0"), "got: {err}");
        assert!(err.contains("simulated 429 rate-limit"), "got: {err}");
    }
}
