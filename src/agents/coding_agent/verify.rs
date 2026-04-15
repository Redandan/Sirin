//! Verification + auto-fix phase.
//!
//! After the main ReAct loop finishes:
//!   1. Run `cargo check` via the shell_exec tool (if the allowlist permits).
//!   2. If it fails, re-enter a smaller fix-focused ReAct loop seeded with
//!      the compiler errors, up to `MAX_FIX_ATTEMPTS` (3) times.
//!   3. If still broken, call [`super::rollback::perform_rollback`] and
//!      return `VerifyOutcome::RolledBack` with the terminal response.
//!
//! The fix loop shares state (`files_modified`, `had_tool_errors`,
//! `last_tool_error`) with the main loop so auto-commit / rollback / the
//! followup reason see everything that happened.

use serde_json::{json, Value};

use crate::adk::AgentContext;
use crate::llm::call_coding_prompt;
use crate::persona::CodingAgentConfig;
use crate::sirin_log;

use super::helpers::{describe_tools, preview_text};
use super::prompt::{build_react_prompt, parse_react_step};
use super::state::RunState;
use super::verdict::HistoryEntry;
use super::{rollback, CodingAgentResponse};

const MAX_FIX_ATTEMPTS: u32 = 3;
const MAX_FIX_PATCH_ERRORS: u32 = 2;
const FIX_ITERATIONS_PER_ATTEMPT: usize = 4;

/// Outcome of the verify phase: either a pass/fail pair the finalize step
/// can consume, or an early-exit `Rollback` response that the caller must
/// return directly.
pub(super) enum VerifyOutcome {
    Verified {
        build_verified: bool,
        verification_output: Option<String>,
    },
    RolledBack(CodingAgentResponse),
}

pub(super) async fn run_verify_and_autofix(
    ctx: &AgentContext,
    project_ctx: &str,
    config: &CodingAgentConfig,
    baseline_commit: Option<&str>,
    dry_run: bool,
    state: &mut RunState,
) -> VerifyOutcome {
    let can_verify = !dry_run
        && config
            .allowed_commands
            .iter()
            .any(|cmd| cmd == "cargo check");

    if !can_verify {
        return VerifyOutcome::Verified {
            build_verified: false,
            verification_output: None,
        };
    }

    let (mut ok, mut out) = verify_build(ctx).await;

    let mut fix_attempt = 0u32;
    while !ok && fix_attempt < MAX_FIX_ATTEMPTS {
        fix_attempt += 1;
        let err_output = out.clone().unwrap_or_default();
        sirin_log!(
            "[coding_agent] cargo check failed (attempt {fix_attempt}/{MAX_FIX_ATTEMPTS}), re-entering ReAct to fix"
        );
        ctx.record_system_event(
            format!("adk_coding_fix_attempt_{fix_attempt}"),
            Some("cargo check failed".to_string()),
            Some("RUNNING"),
            Some(preview_text(&err_output)),
        );

        run_fix_iterations(ctx, project_ctx, &err_output, state).await;

        (ok, out) = verify_build(ctx).await;
    }

    // If still broken after all fix attempts, rollback only the files this
    // task touched — leave any other working-tree changes intact.
    if !ok {
        if let Some(commit) = baseline_commit {
            return VerifyOutcome::RolledBack(rollback::perform_rollback(
                ctx,
                commit,
                &state.files_modified,
                state.history.iter().filter(|h| h.action != "DONE").count(),
                out,
                dry_run,
            ));
        }
    }

    VerifyOutcome::Verified {
        build_verified: ok,
        verification_output: out,
    }
}

/// Run up to `FIX_ITERATIONS_PER_ATTEMPT` ReAct iterations focused on fixing
/// the `err_output` cargo-check error.  Seeds history with the pinned
/// file_read entries from the main loop so the LLM doesn't re-read files it
/// already knows about.  Writes back to `state.files_modified`,
/// `state.had_tool_errors`, `state.last_tool_error`.
async fn run_fix_iterations(
    ctx: &AgentContext,
    project_ctx: &str,
    err_output: &str,
    state: &mut RunState,
) {
    let fix_task = format!(
        "cargo check failed with the following errors. Fix them without changing behaviour:\n\n{}",
        err_output.chars().take(1200).collect::<String>()
    );
    let fix_plan =
        "1. Read the failing file(s)\n2. Apply file_patch to fix the error\n3. DONE".to_string();
    let fix_tool_list = describe_tools();

    let mut fix_history: Vec<HistoryEntry> = state
        .history
        .iter()
        .filter(|h| h.pinned)
        .map(|h| HistoryEntry {
            thought: h.thought.clone(),
            action: h.action.clone(),
            action_input: h.action_input.clone(),
            observation: h.observation.clone(),
            pinned: true,
        })
        .collect();

    let mut fix_patch_errors: u32 = 0;
    for fix_iter in 0..FIX_ITERATIONS_PER_ATTEMPT {
        let fix_window: Vec<&HistoryEntry> = fix_history.iter().collect();
        let prompt = build_react_prompt(
            &fix_task,
            project_ctx,
            &fix_plan,
            &fix_window,
            &fix_tool_list,
            false,
        );
        let raw = match call_coding_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt).await {
            Ok(r) => r,
            Err(e) => {
                sirin_log!("[coding_agent] fix iter {fix_iter} LLM error: {e}");
                break;
            }
        };
        let step = parse_react_step(&raw);
        if step.parse_error {
            let obs = "ERROR: Invalid JSON from model during fix loop. Retry with ONLY the required JSON object.".to_string();
            state.had_tool_errors = true;
            state.last_tool_error = Some(preview_text(&obs));
            fix_history.push(HistoryEntry {
                thought: step.thought,
                action: "INVALID_JSON".to_string(),
                action_input: json!({}),
                observation: obs,
                pinned: false,
            });
            continue;
        }
        if step.action == "DONE" {
            break;
        }

        // Safety: stop fix loop if write patches keep failing.
        if fix_patch_errors >= MAX_FIX_PATCH_ERRORS
            && matches!(
                step.action.as_str(),
                "file_patch" | "file_write" | "plan_execute"
            )
        {
            sirin_log!(
                "[coding_agent] fix loop circuit breaker: {} consecutive patch errors, aborting",
                fix_patch_errors
            );
            break;
        }

        let is_read = step.action == "local_file_read";
        let action_name = step.action.clone();
        let outcome =
            super::step::execute_tool(ctx, &step.action, step.action_input.clone(), is_read).await;
        let obs = outcome.observation;

        if obs.starts_with("ERROR:") {
            if action_name == "file_patch" {
                fix_patch_errors += 1;
            }
        } else {
            // Track files modified in fix loop so auto-commit and
            // rollback cover them too.
            for path in outcome.files_modified {
                if !state.files_modified.contains(&path) {
                    state.files_modified.push(path);
                }
            }
            fix_patch_errors = 0;
        }
        sirin_log!(
            "[coding_agent] fix iter {fix_iter} action={action_name} obs={}",
            preview_text(&obs)
        );
        fix_history.push(HistoryEntry {
            thought: step.thought,
            action: step.action,
            action_input: step.action_input,
            observation: obs,
            pinned: is_read,
        });
    }
}

async fn verify_build(ctx: &AgentContext) -> (bool, Option<String>) {
    match ctx
        .call_tool("shell_exec", json!({ "command": "cargo check" }))
        .await
    {
        Ok(v) => {
            let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);
            let output = format!(
                "exit_code={}\nstdout={}\nstderr={}",
                v.get("exit_code").and_then(Value::as_i64).unwrap_or(-1),
                v.get("stdout").and_then(Value::as_str).unwrap_or(""),
                v.get("stderr").and_then(Value::as_str).unwrap_or(""),
            );
            (success, Some(output))
        }
        Err(e) => (false, Some(format!("shell_exec error: {e}"))),
    }
}
