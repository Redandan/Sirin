//! Local AI Coding Agent — executes a ReAct (Reason + Act) loop to read,
//! modify, and verify code using the tool registry.
//!
//! ## Workflow
//! 1. **Understand** the codebase with `project_overview` + `codebase_search`.
//! 2. **Plan** — single LLM call that produces a numbered step list.
//! 3. **ReAct loop** (max `max_iterations` rounds):
//!    - Prompt the LLM with the task, history, and available tools.
//!    - Parse a JSON response: `{ "thought", "action", "action_input" }`.
//!    - Execute the tool and append the observation.
//!    - If `action == "DONE"`, break and surface the `final_answer`.
//! 4. **Verify** with `cargo check` (if allowed) and collect `git diff`.
//! 5. Return a [`CodingAgentResponse`].

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::llm::call_coding_prompt;
use crate::memory::load_recent_context;
use crate::persona::{CodingAgentConfig, Persona, TaskTracker};
use crate::sirin_log;

// ── Public request / response types ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingRequest {
    /// Natural-language description of the coding task.
    pub task: String,
    /// Override `max_iterations` from persona config.
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// When true, `file_write` calls use `dry_run = true` so nothing is written
    /// to disk.  The agent still produces the intended diff as output.
    #[serde(default)]
    pub dry_run: bool,
    /// Optional conversation context injected by the Router (recent memory turns).
    /// Appended to the project context so the agent is aware of prior discussion.
    #[serde(default)]
    pub context_block: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodingResultStatus {
    #[default]
    Done,
    Verified,
    DryRunDone,
    FollowupNeeded,
    Rollback,
    Error,
}

impl CodingResultStatus {
    pub fn task_status(self) -> &'static str {
        match self {
            Self::Done | Self::Verified | Self::DryRunDone => "DONE",
            Self::FollowupNeeded => "FOLLOWUP_NEEDED",
            Self::Rollback => "ROLLBACK",
            Self::Error => "ERROR",
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Done | Self::Verified | Self::DryRunDone)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodingAgentResponse {
    /// Human-readable summary of what was accomplished.
    pub outcome: String,
    /// Structured result state for UI / task tracking.
    #[serde(default)]
    pub result_status: CodingResultStatus,
    /// Short change summary for the UI/task board.
    #[serde(default)]
    pub change_summary: String,
    /// Files that were (or would have been, in dry-run mode) written.
    pub files_modified: Vec<String>,
    /// Number of ReAct iterations consumed.
    pub iterations_used: usize,
    /// Output of `git diff HEAD` after the task (empty if nothing changed).
    #[serde(default)]
    pub diff: Option<String>,
    /// Whether `cargo check` passed after the changes.
    #[serde(default)]
    pub verified: bool,
    /// Raw output of the verification command.
    #[serde(default)]
    pub verification_output: Option<String>,
    /// Step-by-step execution trace (thought → tool → observation).
    #[serde(default)]
    pub trace: Vec<String>,
    /// True when the agent ran in dry-run mode.
    #[serde(default)]
    pub dry_run: bool,
}

// ── Agent struct ──────────────────────────────────────────────────────────────

pub struct CodingAgent;

impl Agent for CodingAgent {
    fn name(&self) -> &'static str {
        "coding_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: CodingRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid coding request payload: {e}"))?;

            let config = Persona::load().map(|p| p.coding_agent).unwrap_or_default();

            if !config.enabled {
                return Err("Coding agent is disabled in persona config.".to_string());
            }

            let response = run_react_loop(ctx, &request, &config).await?;
            serde_json::to_value(response).map_err(|e| e.to_string())
        }
        .boxed()
    }
}

// ── ReAct loop ────────────────────────────────────────────────────────────────

async fn run_react_loop(
    ctx: &AgentContext,
    request: &CodingRequest,
    config: &CodingAgentConfig,
) -> Result<CodingAgentResponse, String> {
    let max_iter = request.max_iterations.unwrap_or(config.max_iterations);
    let dry_run = request.dry_run || !config.auto_approve_writes;
    let read_only_analysis = is_read_only_analysis_task(&request.task);

    sirin_log!(
        "[coding_agent] task='{}' max_iter={max_iter} dry_run={dry_run}",
        request.task
    );
    ctx.record_system_event(
        "adk_coding_agent_start",
        Some(preview_text(&request.task)),
        Some("RUNNING"),
        Some(format!("max_iter={max_iter} dry_run={dry_run}")),
    );

    // ── Step 1: gather project context ────────────────────────────────────────
    let mut project_ctx = gather_project_context(ctx, &request.task).await;

    // Append recent conversation context if available so the agent has
    // awareness of what the user was just discussing.
    if let Some(hint) = ctx.context_hint() {
        project_ctx.push_str("\n\n## Recent conversation context\n");
        project_ctx.push_str(hint);
    }

    // Append router-injected memory context when provided.
    // This is the conversation history the Router fetched from SQLite memory
    // before dispatching, giving the agent cross-turn awareness without an
    // additional database query.
    if let Some(block) = &request.context_block {
        if !block.trim().is_empty() {
            project_ctx.push_str("\n\n## Conversation memory (router-injected)\n");
            project_ctx.push_str(block);
        }
    }

    // ── Step 2: planning call ─────────────────────────────────────────────────
    let plan = make_plan(ctx, &request.task, &project_ctx).await;
    sirin_log!("[coding_agent] plan ready ({} chars)", plan.len());

    // ── Step 2.5: record baseline commit (rollback anchor) ───────────────────
    // Call git directly — bypasses the shell_exec allowlist which only permits
    // cargo commands. Never panics; baseline is simply None if git is absent.
    let baseline_commit = if !dry_run {
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    // ── Step 3: ReAct loop ────────────────────────────────────────────────────
    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut files_modified: Vec<String> = Vec::new();
    let mut final_answer = String::new();
    let mut attempted_write = false;
    let mut had_tool_errors = false;
    let mut last_tool_error: Option<String> = None;
    let mut total_tool_errors: u32 = 0;
    let mut stalled_iterations: u32 = 0;
    let mut last_step_fingerprint: Option<String> = None;
    let tool_list = describe_tools();

    // Safety counters — prevent destructive escalation when surgical edits fail.
    let mut consecutive_patch_errors: u32 = 0;
    const MAX_PATCH_ERRORS: u32 = 2;
    const MAX_TOTAL_TOOL_ERRORS: u32 = 3;
    let max_stalled_iterations: u32 = if read_only_analysis { 2 } else { 3 };
    let write_tools = ["file_write", "file_patch", "plan_execute"];

    // Read cache: deduplicates local_file_read calls within one task so the
    // model can't waste iterations re-reading files it already inspected.
    // Key = file path.  Value = (first_read_iteration, formatted_content).
    // Invalidated when file_patch / file_write succeeds on that path.
    let mut file_read_cache: std::collections::HashMap<String, (usize, String)> =
        std::collections::HashMap::new();

    // Sliding window: only pass recent N entries to the LLM.
    // file_read entries are "pinned" and kept, but capped at MAX_PINNED to
    // prevent context explosion when many files are read in one task.
    // When over the cap, keep only the most recent pinned entries.
    const HISTORY_WINDOW: usize = 6;
    const MAX_PINNED: usize = 4;

    for iteration in 0..max_iter {
        // Build history window: capped pinned entries + recent N non-pinned.
        let history_window: Vec<&HistoryEntry> = {
            let mut pinned: Vec<&HistoryEntry> = history.iter().filter(|h| h.pinned).collect();
            // Keep only the most recent MAX_PINNED file reads.
            if pinned.len() > MAX_PINNED {
                pinned = pinned[pinned.len() - MAX_PINNED..].to_vec();
            }
            let recent: Vec<&HistoryEntry> = history
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
            &project_ctx,
            &plan,
            &history_window,
            &tool_list,
            dry_run,
        );

        let raw = match call_coding_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt).await {
            Ok(raw) => raw,
            Err(e) => {
                let err_msg = format!("LLM error on iteration {iteration}: {e}");
                if read_only_analysis && has_sufficient_analysis_evidence(&history) {
                    had_tool_errors = true;
                    last_tool_error = Some(preview_text(&err_msg));
                    final_answer =
                        format!("⚠️ {err_msg}\n\n{}", synthesize_read_only_outcome(&history));
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
            if read_only_analysis && has_sufficient_analysis_evidence(&history) {
                final_answer = salvage_non_json_final_answer(&raw, &history);
                history.push(HistoryEntry {
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
            history.push(HistoryEntry {
                thought: step.thought,
                action: "INVALID_JSON".to_string(),
                action_input: json!({}),
                observation: observation.clone(),
                pinned: false,
            });
            had_tool_errors = true;
            total_tool_errors += 1;
            last_tool_error = Some(preview_text(&observation));

            if total_tool_errors >= MAX_TOTAL_TOOL_ERRORS {
                final_answer = build_fail_fast_outcome(
                    "模型連續回傳不可解析的輸出，已觸發 fail-fast",
                    &history,
                    last_tool_error.as_deref(),
                    read_only_analysis,
                );
                ctx.record_system_event(
                    "adk_coding_fail_fast",
                    Some("⚠ fail-fast：模型連續回傳無效 JSON".to_string()),
                    Some("FOLLOWUP_NEEDED"),
                    last_tool_error.clone(),
                );
                break;
            }
            continue;
        }

        if step.action == "DONE" {
            final_answer = step.final_answer.unwrap_or_else(|| step.thought.clone());
            history.push(HistoryEntry {
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
            final_answer = format!(
                "任務中止：file_patch 連續失敗 {} 次，已封鎖所有寫入工具以防止資料損毀。\
                請縮小任務範圍，並確認目標函式的確切位置後重試。",
                consecutive_patch_errors
            );
            break;
        }

        // Execute the tool.
        // In dry_run mode, force dry_run=true into every write tool so that
        // writes are never applied regardless of how the agent calls them.
        // file_patch, file_write, and plan_execute all honour the dry_run flag.
        let is_write_tool = matches!(
            step.action.as_str(),
            "file_write" | "file_patch" | "plan_execute"
        );
        if is_write_tool {
            attempted_write = true;
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
                match ctx.call_tool(&step.action, tool_input).await {
                    Ok(v) => {
                        let out = format_tool_output_large(&v);
                        if !path_key.is_empty() {
                            file_read_cache.insert(path_key, (iteration, out.clone()));
                        }
                        out
                    }
                    Err(e) => format!("ERROR: {e}"),
                }
            }
        } else {
            match ctx.call_tool(&step.action, tool_input).await {
                Ok(v) => {
                    // Track which files were written.
                    if step.action == "file_write" || step.action == "file_patch" {
                        if let Some(path) = v.get("path").and_then(Value::as_str) {
                            if !files_modified.contains(&path.to_string()) {
                                files_modified.push(path.to_string());
                            }
                        }
                    }
                    // Track files touched via plan_execute steps.
                    if step.action == "plan_execute" {
                        if let Some(results) = v.get("results").and_then(Value::as_array) {
                            for r in results {
                                if let Some(result) = r.get("result") {
                                    if let Some(path) = result.get("path").and_then(Value::as_str) {
                                        if !files_modified.contains(&path.to_string()) {
                                            files_modified.push(path.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    format_tool_output(&v)
                }
                Err(e) => format!("ERROR: {e}"),
            }
        };

        let action_name = step.action.clone();
        let observation = maybe_enrich_tool_error(&action_name, observation);
        history.push(HistoryEntry {
            thought: step.thought,
            action: step.action,
            action_input: step.action_input,
            observation: observation.clone(),
            pinned: is_file_read,
        });

        // Track consecutive patch errors for safety circuit breaker.
        if observation.starts_with("ERROR:") {
            had_tool_errors = true;
            total_tool_errors += 1;
            last_tool_error = Some(preview_text(&observation));
            sirin_log!("[coding_agent] tool error: {observation}");
            if action_name == "file_patch" {
                consecutive_patch_errors += 1;
            }
        } else if action_name == "file_patch" {
            consecutive_patch_errors = 0; // reset on success
        }

        let fingerprint = step_fingerprint(
            &action_name,
            &history
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

        if read_only_analysis && repeated_cache_hit && has_sufficient_analysis_evidence(&history) {
            final_answer = synthesize_read_only_outcome(&history);
            ctx.record_system_event(
                "adk_coding_fail_fast",
                Some("✅ read-only analysis：grounded answer ready".to_string()),
                Some("DONE"),
                Some("Stopped after repeated cached reads because enough grounded evidence was already collected.".to_string()),
            );
            break;
        }

        if total_tool_errors >= MAX_TOTAL_TOOL_ERRORS {
            final_answer = build_fail_fast_outcome(
                "工具錯誤次數過多，已觸發 fail-fast",
                &history,
                last_tool_error.as_deref(),
                read_only_analysis,
            );
            ctx.record_system_event(
                "adk_coding_fail_fast",
                Some("⚠ fail-fast：工具錯誤次數過多".to_string()),
                Some("FOLLOWUP_NEEDED"),
                last_tool_error.clone(),
            );
            break;
        }

        if stalled_iterations >= max_stalled_iterations {
            final_answer = build_fail_fast_outcome(
                "連續多步沒有新進展，已觸發 fail-fast",
                &history,
                last_tool_error.as_deref(),
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

    // ── Step 4: verification + auto-fix loop ─────────────────────────────────
    let can_verify = !dry_run
        && config
            .allowed_commands
            .iter()
            .any(|cmd| cmd == "cargo check");
    let (build_verified, verification_output) = if can_verify {
        let (mut ok, mut out) = verify_build(ctx).await;

        // If verification fails, re-enter the ReAct loop up to 3 times to fix
        // the compilation errors before giving up.
        const MAX_FIX_ATTEMPTS: u32 = 3;
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

            // Inject the compiler error as context and run fix iterations.
            let fix_task = format!(
                "cargo check failed with the following errors. Fix them without changing behaviour:\n\n{}",
                err_output.chars().take(1200).collect::<String>()
            );
            let fix_plan =
                "1. Read the failing file(s)\n2. Apply file_patch to fix the error\n3. DONE"
                    .to_string();
            let fix_tool_list = describe_tools();

            // Seed fix history with the pinned file_read entries from the main
            // loop so the LLM doesn't have to re-read files it already knows.
            let mut fix_history: Vec<HistoryEntry> = history
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

            // Re-run up to 4 ReAct iterations to apply the fix.
            // Circuit breaker: abort if file_patch fails twice in a row.
            let mut fix_patch_errors: u32 = 0;
            const MAX_FIX_PATCH_ERRORS: u32 = 2;
            for fix_iter in 0..4usize {
                let fix_window: Vec<&HistoryEntry> = fix_history.iter().collect();
                let prompt = build_react_prompt(
                    &fix_task,
                    &project_ctx,
                    &fix_plan,
                    &fix_window,
                    &fix_tool_list,
                    false,
                );
                let raw =
                    match call_coding_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt).await {
                        Ok(r) => r,
                        Err(e) => {
                            sirin_log!("[coding_agent] fix iter {fix_iter} LLM error: {e}");
                            break;
                        }
                    };
                let step = parse_react_step(&raw);
                if step.parse_error {
                    let obs = "ERROR: Invalid JSON from model during fix loop. Retry with ONLY the required JSON object.".to_string();
                    had_tool_errors = true;
                    last_tool_error = Some(preview_text(&obs));
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
                let obs = match ctx.call_tool(&step.action, step.action_input.clone()).await {
                    Ok(v) => {
                        // Track files modified in fix loop so auto-commit and
                        // rollback cover them too.
                        if action_name == "file_write" || action_name == "file_patch" {
                            if let Some(path) = v.get("path").and_then(Value::as_str) {
                                if !files_modified.contains(&path.to_string()) {
                                    files_modified.push(path.to_string());
                                }
                            }
                        }
                        if action_name == "plan_execute" {
                            if let Some(results) = v.get("results").and_then(Value::as_array) {
                                for r in results {
                                    if let Some(result) = r.get("result") {
                                        if let Some(path) =
                                            result.get("path").and_then(Value::as_str)
                                        {
                                            if !files_modified.contains(&path.to_string()) {
                                                files_modified.push(path.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        fix_patch_errors = 0;
                        if is_read {
                            format_tool_output_large(&v)
                        } else {
                            format_tool_output(&v)
                        }
                    }
                    Err(e) => {
                        if action_name == "file_patch" {
                            fix_patch_errors += 1;
                        }
                        format!("ERROR: {e}")
                    }
                };
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

            (ok, out) = verify_build(ctx).await;
        }

        // If still broken after all fix attempts, rollback only the files this
        // task touched — leave any other working-tree changes intact.
        if !ok {
            if let Some(ref commit) = baseline_commit {
                sirin_log!(
                    "[coding_agent] ROLLBACK: restoring {} file(s) to {commit}",
                    files_modified.len()
                );
                let mut rolled_back = Vec::new();
                let mut rollback_errors = Vec::new();

                for path in &files_modified {
                    // Use `git show {commit}:{path}` to get the file content at
                    // baseline — this never touches the allowlist.
                    let result = std::process::Command::new("git")
                        .args(["show", &format!("{commit}:{path}")])
                        .output();

                    match result {
                        Ok(out) if out.status.success() => {
                            let content = out.stdout;
                            match std::fs::write(path, &content) {
                                Ok(_) => {
                                    sirin_log!("[coding_agent] ROLLBACK: restored {path}");
                                    rolled_back.push(path.clone());
                                }
                                Err(e) => {
                                    rollback_errors.push(format!("{path}: write failed ({e})"));
                                }
                            }
                        }
                        Ok(out) => {
                            // File didn't exist at baseline commit — delete it.
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            if stderr.contains("does not exist")
                                || stderr.contains("exists on disk")
                            {
                                let _ = std::fs::remove_file(path);
                                rolled_back.push(path.clone());
                            } else {
                                rollback_errors
                                    .push(format!("{path}: git show failed ({})", stderr.trim()));
                            }
                        }
                        Err(e) => {
                            rollback_errors.push(format!("{path}: git show error ({e})"));
                        }
                    }
                }

                ctx.record_system_event(
                    "adk_coding_rollback",
                    Some(format!(
                        "已回滾 {} 個檔案到 {}",
                        rolled_back.len(),
                        &commit[..8.min(commit.len())]
                    )),
                    Some("ROLLBACK"),
                    Some(format!(
                        "restored={} errors={}",
                        rolled_back.join(","),
                        if rollback_errors.is_empty() {
                            "none".to_string()
                        } else {
                            rollback_errors.join(";")
                        }
                    )),
                );

                let rollback_msg = if rollback_errors.is_empty() {
                    format!(
                        "⚠️ cargo check 仍失敗，已自動還原 {} 個檔案到 commit {}。請檢查任務描述後重試。\n還原檔案：{}",
                        rolled_back.len(),
                        &commit[..8.min(commit.len())],
                        rolled_back.join(", ")
                    )
                } else {
                    format!(
                        "⚠️ cargo check 仍失敗，部分還原失敗。成功：{} 失敗：{}",
                        rolled_back.join(", "),
                        rollback_errors.join("; ")
                    )
                };

                return Ok(CodingAgentResponse {
                    outcome: rollback_msg,
                    result_status: CodingResultStatus::Rollback,
                    change_summary: "已自動回滾本次修改，請重新確認任務描述後再試。".to_string(),
                    files_modified: vec![],
                    iterations_used: history.iter().filter(|h| h.action != "DONE").count(),
                    diff: None,
                    verified: false,
                    verification_output: out,
                    trace: vec![],
                    dry_run,
                });
            }
        }

        (ok, out)
    } else {
        (false, None)
    };

    let verified = overall_verified(
        dry_run,
        build_verified,
        attempted_write,
        files_modified.len(),
        had_tool_errors,
    );

    // ── Step 5: diff ──────────────────────────────────────────────────────────
    let diff = if !dry_run { get_diff(ctx).await } else { None };

    // ── Step 6: auto-commit when verified ────────────────────────────────────
    // Only commit when: not dry_run, cargo check passed, files were actually
    // changed, and git is available. Uses the task description as commit msg.
    let auto_committed = if !dry_run && verified && !files_modified.is_empty() {
        auto_commit(&request.task, &files_modified).await
    } else {
        false
    };

    let iterations_used = history.iter().filter(|h| h.action != "DONE").count();
    let trace: Vec<String> = history
        .iter()
        .map(|h| {
            format!(
                "💭 {}\n🔧 {}({})\n👁 {}",
                preview_text(&h.thought),
                h.action,
                preview_tool_input(&h.action_input),
                preview_text(&h.observation)
            )
        })
        .collect();

    let mut outcome = if final_answer.is_empty() {
        if read_only_analysis && has_sufficient_analysis_evidence(&history) {
            synthesize_read_only_outcome(&history)
        } else {
            format!(
                "Completed {iterations_used} step(s). Files touched: {}",
                if files_modified.is_empty() {
                    "none".to_string()
                } else {
                    files_modified.join(", ")
                }
            )
        }
    } else {
        final_answer
    };

    if let Some(reason) = followup_reason(
        dry_run,
        build_verified,
        attempted_write,
        files_modified.len(),
        had_tool_errors,
        last_tool_error.as_deref(),
    ) {
        outcome = format!("⚠️ {reason}\n\n{outcome}");
    }

    let outcome = if auto_committed {
        format!("{outcome}\n\n✅ 已自動 commit（cargo check 通過）")
    } else {
        outcome
    };

    let change_summary =
        build_change_summary(&files_modified, verified, dry_run, auto_committed, &outcome);
    let analysis_completed = read_only_analysis && has_sufficient_analysis_evidence(&history);
    let result_status = derive_result_status(
        dry_run,
        analysis_completed,
        verified,
        build_verified,
        attempted_write,
        files_modified.len(),
        had_tool_errors,
    );

    ctx.record_system_event(
        "adk_coding_agent_done",
        Some(change_summary.clone()),
        Some(result_status.task_status()),
        Some(format!(
            "status={:?}; summary={change_summary}; files={} verified={verified} committed={auto_committed} dry_run={dry_run}; outcome={}",
            result_status,
            files_modified.len(),
            preview_text(&outcome)
        )),
    );

    Ok(CodingAgentResponse {
        outcome,
        result_status,
        change_summary,
        files_modified,
        iterations_used,
        diff,
        verified,
        verification_output,
        trace,
        dry_run,
    })
}

// ── Context & planning ────────────────────────────────────────────────────────

async fn gather_project_context(ctx: &AgentContext, task: &str) -> String {
    let overview = ctx
        .call_tool("project_overview", json!({}))
        .await
        .ok()
        .and_then(|v| {
            v.get("summary")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    let path_hints = extract_path_hints_from_task(task);
    let search_query = path_hints
        .first()
        .cloned()
        .unwrap_or_else(|| task.chars().take(60).collect());
    let search = ctx
        .call_tool(
            "codebase_search",
            json!({ "query": search_query, "limit": 4 }),
        )
        .await
        .ok()
        .map(|v| format_tool_output(&v))
        .unwrap_or_default();

    let explicit_file_context = build_task_named_file_context(&path_hints);

    format!(
        "Project overview: {overview}\n\nRelevant codebase context:\n{search}{explicit_file_context}"
    )
}

fn extract_path_hints_from_task(task: &str) -> Vec<String> {
    let mut hints = Vec::new();
    let known_exts = [
        ".rs", ".toml", ".md", ".json", ".yaml", ".yml", ".tsx", ".ts", ".js",
    ];

    for raw in task.split_whitespace() {
        let trimmed = raw
            .trim()
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '"'
                        | '\''
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | ','
                        | '，'
                        | '。'
                        | ':'
                        | '：'
                        | ';'
                        | '；'
                        | '?'
                        | '？'
                )
            })
            .replace('\\', "/");
        let cleaned: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
            .collect();

        if cleaned.is_empty() {
            continue;
        }

        let looks_like_path = cleaned.contains('/')
            || cleaned.starts_with("src")
            || cleaned.starts_with("tests")
            || cleaned.starts_with("app")
            || cleaned.starts_with("config");
        let has_known_ext = known_exts.iter().any(|ext| cleaned.ends_with(ext));

        if (looks_like_path || has_known_ext) && !hints.contains(&cleaned) {
            hints.push(cleaned);
        }

        if hints.len() >= 3 {
            break;
        }
    }

    hints
}

fn build_task_named_file_context(path_hints: &[String]) -> String {
    let blocks: Vec<String> = path_hints
        .iter()
        .take(2)
        .filter_map(|path| {
            crate::memory::inspect_project_file_range(path, Some(1), Some(120), 5000)
                .ok()
                .map(|content| {
                    format!("\n\n## Task-named file hint\nRequested path: {path}\n{content}")
                })
        })
        .collect();

    blocks.join("")
}

fn is_read_only_analysis_task(task: &str) -> bool {
    let lower = task.to_lowercase();
    let asks_analysis = [
        "分析", "說明", "summar", "explain", "inspect", "review", "找出", "檢查", "read ",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let forbids_write = [
        "不要寫入",
        "不要修改",
        "do not modify",
        "don't modify",
        "read-only",
        "dry-run",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let asks_to_change = [
        "修改",
        "新增",
        "加入",
        "修正",
        "fix",
        "implement",
        "write",
        "patch",
        "refactor",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    forbids_write || (asks_analysis && !asks_to_change)
}

async fn make_plan(ctx: &AgentContext, task: &str, project_ctx: &str) -> String {
    let prompt = format!(
        "You are an expert software engineer. \
Plan the minimal steps to complete this coding task.\n\n\
Task: {task}\n\n\
{project_ctx}\n\n\
List 3-6 numbered steps. Be specific about which files to read or modify. \
Return only the numbered list, no extra prose.",
    );
    // Plan generation is a lightweight step-list task — use the router (local)
    // LLM to save remote quota for the actual ReAct coding iterations.
    crate::llm::call_prompt(ctx.http.as_ref(), &crate::llm::shared_router_llm(), prompt)
        .await
        .unwrap_or_else(|_| "1. Read relevant files\n2. Make changes\n3. Verify".to_string())
}

// ── ReAct prompt ──────────────────────────────────────────────────────────────

fn describe_tools() -> String {
    let tools = [
        ("file_list", r#"{"path":"dir","max_depth":3}"#, "List files in a directory."),
        ("local_file_read", r#"{"path":"src/foo.rs","start_line":100,"end_line":200}"#, "Read a file's content. Use start_line/end_line (1-based, optional) to fetch a specific window — output includes line numbers, essential for precise file_patch old_str."),
        ("file_write", r#"{"path":"src/foo.rs","content":"..."}"#, "Write full content to a file (use only when replacing the entire file)."),
        ("file_patch", r#"{"path":"src/foo.rs","hunks":[{"old_str":"fn foo() {","new_str":"fn foo() -> i32 {"}]}"#, "Apply surgical hunk-based edits. Fails atomically if any old_str is not found. Prefer over file_write for partial changes."),
        ("file_diff", r#"{"path":null}"#, "Show git diff of uncommitted changes."),
        ("shell_exec", r#"{"command":"cargo check"}"#, "Run a whitelisted shell command."),
        ("codebase_search", r#"{"query":"...","limit":5}"#, "Search codebase for relevant code."),
        ("symbol_search", r#"{"query":"function_name"}"#, "Search for a symbol by name."),
        ("call_graph_query", r#"{"symbol":"my_fn","hops":1}"#, "Look up callers and callees of a symbol in the call graph."),
        ("plan_execute", r#"{"steps":[{"tool":"file_patch","input":{...}},{"tool":"shell_exec","input":{"command":"cargo check"}}]}"#, "Execute multiple tool steps in sequence. Stops on first failure. Use to batch multi-file changes in one action."),
        ("git_status", r#"{}"#, "Show git status."),
        ("git_log", r#"{"limit":5}"#, "Show recent git commits."),
        ("memory_search", r#"{"query":"...","limit":3}"#, "Search past memories."),
    ];
    tools
        .iter()
        .map(|(name, example, desc)| format!("- `{name}({example})`: {desc}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Soft character limit for a single ReAct prompt.  LM Studio at 32K context ≈
/// 128K chars (4 chars/token estimate).  We leave headroom for the LLM output.
const MAX_PROMPT_CHARS: usize = 20_000;

fn build_react_prompt(
    task: &str,
    project_ctx: &str,
    plan: &str,
    history: &[&HistoryEntry],
    tool_list: &str,
    dry_run: bool,
) -> String {
    let dry_run_note = if dry_run {
        "\nNOTE: Running in DRY-RUN mode. For file_write, file_patch, and plan_execute actions \
pass `\"dry_run\": true` in action_input. Files will NOT be written to disk; the agent will \
report what would change.\n"
    } else {
        ""
    };
    let analysis_mode_note = if is_read_only_analysis_task(task) {
        "\nREAD-ONLY ANALYSIS MODE: inspect the most relevant 2-4 files, then return `DONE` with a concise evidence-based summary that cites the file paths you used. Avoid repeating the same reads/searches once the answer is clear.\n"
    } else {
        ""
    };

    let history_block = if history.is_empty() {
        String::new()
    } else {
        let entries: Vec<String> = history
            .iter()
            .map(|h| {
                format!(
                    "Thought: {}\nAction: {}\nAction Input: {}\nObservation: {}",
                    h.thought,
                    h.action,
                    serde_json::to_string(&h.action_input).unwrap_or_default(),
                    h.observation
                )
            })
            .collect();
        format!("\n## Previous steps\n{}\n", entries.join("\n---\n"))
    };

    // Token budget: estimate static sections and trim project_ctx if necessary
    // so the full prompt stays under MAX_PROMPT_CHARS.  Keeps the LLM from
    // receiving a truncated prompt silently when context is large.
    let static_budget = task.len() + plan.len() + tool_list.len()
        + history_block.len() + 800 /* boilerplate */;
    let ctx_budget = MAX_PROMPT_CHARS.saturating_sub(static_budget);
    let project_ctx_trimmed: String = project_ctx.chars().take(ctx_budget.max(400)).collect();

    format!(
        r#"You are Sirin, a local AI Coding Agent.
{dry_run_note}
## Task
{task}

## Plan
{plan}

## Project context
{project_ctx_trimmed}
{history_block}
## Available tools
{tool_list}

## Instructions
Decide the next single action to take.{analysis_mode_note}

**Tool preferences:**
- Prefer `file_patch` over `file_write` whenever you are making partial changes to an existing file. `file_patch` is surgical and safe — it fails atomically if the context doesn't match, preventing accidental corruption.
- Before any write, first confirm the exact target path with `local_file_read`, `file_list`, or `codebase_search`. In Rust projects, `foo.rs` may actually live at `foo/mod.rs`.
- If the task explicitly names a file path, inspect that path first and treat the resolved file as primary evidence.
- If a read/patch returns `not found` or `old_str` mismatch, do NOT say `DONE`. Re-discover the real path, re-read the latest file, and then retry the surgical patch.
- If the requested behavior is already present, cite the confirming file/path evidence in `final_answer` instead of forcing another edit.
- Use `plan_execute` when the task requires changes to multiple files — batch all the `file_patch` (and optionally a final `shell_exec`) calls into one `plan_execute` action to complete the work in a single step.
- Use `call_graph_query` to understand callers and callees before modifying a function.

Respond with ONLY valid JSON in this exact format (no markdown fences):
{{
  "thought": "your reasoning about what to do next",
  "action": "tool_name or DONE",
  "action_input": {{}},
  "final_answer": "summary of what you accomplished (only when action is DONE)"
}}

If you have finished ALL steps in the plan and the task is complete, set action to "DONE".
"#
    )
}

// ── Step parsing ──────────────────────────────────────────────────────────────

struct ReactStep {
    thought: String,
    action: String,
    action_input: Value,
    final_answer: Option<String>,
    parse_error: bool,
}

fn extract_json_body(raw: &str) -> &str {
    // Strip markdown code fences first.
    let s = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    // Then extract the outermost { … } so any preamble/postamble text is ignored.
    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        if start <= end {
            return &s[start..=end];
        }
    }
    s
}

fn parse_react_step(raw: &str) -> ReactStep {
    let cleaned = extract_json_body(raw);

    if let Ok(v) = serde_json::from_str::<Value>(cleaned) {
        let thought = v
            .get("thought")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let action = v
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("DONE")
            .to_string();
        let action_input = v.get("action_input").cloned().unwrap_or(json!({}));
        let final_answer = v
            .get("final_answer")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        return ReactStep {
            thought,
            action,
            action_input,
            final_answer,
            parse_error: false,
        };
    }

    // Fallback: LLM didn't produce valid JSON — request another iteration
    // instead of falsely treating the task as complete.
    ReactStep {
        thought: format!(
            "(LLM response could not be parsed as JSON) raw={}",
            preview_text(raw)
        ),
        action: "INVALID_JSON".to_string(),
        action_input: json!({}),
        final_answer: None,
        parse_error: true,
    }
}

// ── Post-processing ───────────────────────────────────────────────────────────

fn overall_verified(
    dry_run: bool,
    build_verified: bool,
    attempted_write: bool,
    files_modified_count: usize,
    had_tool_errors: bool,
) -> bool {
    if dry_run || !build_verified {
        return false;
    }
    if attempted_write && files_modified_count == 0 {
        return false;
    }
    if had_tool_errors && files_modified_count == 0 {
        return false;
    }
    true
}

fn followup_reason(
    dry_run: bool,
    build_verified: bool,
    attempted_write: bool,
    files_modified_count: usize,
    had_tool_errors: bool,
    last_tool_error: Option<&str>,
) -> Option<String> {
    if dry_run {
        return None;
    }

    if !build_verified {
        return Some("cargo check 尚未通過，任務仍需 follow-up。".to_string());
    }

    if attempted_write && files_modified_count == 0 {
        let suffix = last_tool_error
            .map(|err| format!(" 最後錯誤：{err}"))
            .unwrap_or_default();
        return Some(format!(
            "Agent 曾嘗試修改程式，但沒有任何檔案真正寫入；這通常代表路徑猜錯、上下文已過期，或 patch 比對失敗。請先重新確認真實檔案位置後再繼續。{suffix}"
        ));
    }

    if had_tool_errors && files_modified_count == 0 {
        let suffix = last_tool_error
            .map(|err| format!(" 最後錯誤：{err}"))
            .unwrap_or_default();
        return Some(format!(
            "工具執行過程仍有錯誤，任務需要 follow-up。{suffix}"
        ));
    }

    None
}

fn maybe_enrich_tool_error(action_name: &str, observation: String) -> String {
    if !observation.starts_with("ERROR:") {
        return observation;
    }

    let lower = observation.to_lowercase();
    let looks_like_path_issue = lower.contains("could not resolve local project file")
        || lower.contains("cannot read '")
        || lower.contains("patch aborted")
        || lower.contains("not found in '")
        || lower.contains("directory not found");

    if looks_like_path_issue
        && matches!(
            action_name,
            "local_file_read" | "file_patch" | "file_write" | "plan_execute"
        )
    {
        format!(
            "{observation}\nHint: verify the real path with file_list/codebase_search before more writes. In Rust projects, `foo.rs` may actually be `foo/mod.rs`."
        )
    } else {
        observation
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

async fn get_diff(ctx: &AgentContext) -> Option<String> {
    ctx.call_tool("file_diff", json!({}))
        .await
        .ok()
        .and_then(|v| v.get("diff").and_then(Value::as_str).map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
}

/// Truncate `s` to at most `max_bytes` UTF-8 bytes, always at a valid char
/// boundary.  Chinese/CJK chars are 3 bytes each, so 72 bytes ≈ 24 CJK chars.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Stage only the files this task modified and create a commit.
/// Returns true on success. Never panics — all errors are logged and ignored.
async fn auto_commit(task: &str, files_modified: &[String]) -> bool {
    // Stage only the modified files (never `git add -A`).
    let add_ok = std::process::Command::new("git")
        .arg("add")
        .arg("--")
        .args(files_modified)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !add_ok {
        sirin_log!("[coding_agent] auto_commit: git add failed");
        return false;
    }

    // Build a concise commit message.  Truncate at 72 *bytes* (not chars) so
    // CJK-heavy task descriptions don't blow past the conventional line limit.
    let prefix = "chore(sirin-agent): ";
    let max_summary_bytes = 72usize.saturating_sub(prefix.len());
    let summary = truncate_to_bytes(task.trim(), max_summary_bytes);
    let msg = format!(
        "{prefix}{summary}\n\nAuto-committed by Sirin Coding Agent after cargo check passed."
    );

    let commit_ok = std::process::Command::new("git")
        .args(["commit", "-m", &msg])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !commit_ok {
        sirin_log!("[coding_agent] auto_commit: git commit failed (nothing to commit?)");
        return false;
    }

    sirin_log!(
        "[coding_agent] auto_commit: committed {} file(s)",
        files_modified.len()
    );
    true
}

// ── History entry ─────────────────────────────────────────────────────────────

struct HistoryEntry {
    thought: String,
    action: String,
    action_input: Value,
    observation: String,
    /// Pinned entries (e.g. file_read) are always included in the history
    /// window regardless of how many iterations have passed.
    pinned: bool,
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn format_tool_output(v: &Value) -> String {
    match v {
        Value::String(s) => s.chars().take(800).collect(),
        Value::Array(arr) => arr
            .iter()
            .take(10)
            .map(|item| {
                item.as_str()
                    .unwrap_or(&item.to_string())
                    .chars()
                    .take(120)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            let s = serde_json::to_string_pretty(other).unwrap_or_default();
            s.chars().take(800).collect()
        }
    }
}

/// Like `format_tool_output` but with a larger budget (2000 chars) for file_read
/// observations, so the LLM sees enough source context to construct accurate
/// `old_str` for `file_patch`.
fn format_tool_output_large(v: &Value) -> String {
    match v {
        Value::String(s) => s.chars().take(2000).collect(),
        Value::Array(arr) => arr
            .iter()
            .take(20)
            .map(|item| {
                item.as_str()
                    .unwrap_or(&item.to_string())
                    .chars()
                    .take(200)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            let s = serde_json::to_string_pretty(other).unwrap_or_default();
            s.chars().take(2000).collect()
        }
    }
}

fn preview_tool_input(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    s.chars().take(60).collect()
}

fn preview_text(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn step_fingerprint(action_name: &str, action_input: &Value, observation: &str) -> String {
    format!(
        "{}|{}|{}",
        action_name,
        preview_tool_input(action_input),
        preview_text(observation)
    )
}

fn file_read_cache_key(input: &Value) -> String {
    let path = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let start = input.get("start_line").and_then(Value::as_u64).unwrap_or(0);
    let end = input.get("end_line").and_then(Value::as_u64).unwrap_or(0);

    format!("{path}|{start}|{end}")
}

fn has_sufficient_analysis_evidence(history: &[HistoryEntry]) -> bool {
    history
        .iter()
        .filter(|h| h.action == "local_file_read" && !h.observation.starts_with("ERROR:"))
        .count()
        >= 2
}

fn inspected_paths_from_history(history: &[HistoryEntry]) -> Vec<String> {
    let mut paths = Vec::new();
    for entry in history {
        if entry.action != "local_file_read" {
            continue;
        }
        if let Some(path) = entry.action_input.get("path").and_then(Value::as_str) {
            let path = path.trim().to_string();
            if !path.is_empty() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn salvage_non_json_final_answer(raw: &str, history: &[HistoryEntry]) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 40 {
        return trimmed.to_string();
    }

    let inspected = inspected_paths_from_history(history);
    if inspected.is_empty() {
        "分析完成，但模型沒有回傳結構化 JSON。請根據已讀取的檔案內容確認結論。".to_string()
    } else {
        format!(
            "分析完成。已檢查檔案：{}。模型最後一步沒有回傳合法 JSON，但前面的檔案證據已足以支撐這個結論。",
            inspected.join(", ")
        )
    }
}

fn synthesize_read_only_outcome(history: &[HistoryEntry]) -> String {
    let inspected = inspected_paths_from_history(history);
    let evidence = history
        .iter()
        .rev()
        .find(|h| {
            matches!(h.action.as_str(), "local_file_read" | "codebase_search")
                && !h.observation.starts_with("ERROR:")
        })
        .map(|h| preview_text(&h.observation))
        .unwrap_or_default();

    if inspected.is_empty() {
        "分析完成；目前沒有寫入任何檔案。".to_string()
    } else if evidence.is_empty() {
        format!(
            "分析完成。已檢查檔案：{}。目前沒有寫入任何檔案。",
            inspected.join(", ")
        )
    } else {
        format!(
            "分析完成。已檢查檔案：{}。目前沒有寫入任何檔案。\n\n最後一條關鍵證據：{}",
            inspected.join(", "),
            evidence
        )
    }
}

fn build_fail_fast_outcome(
    reason: &str,
    history: &[HistoryEntry],
    last_tool_error: Option<&str>,
    read_only_analysis: bool,
) -> String {
    let suffix = last_tool_error
        .map(|err| format!(" 最後錯誤：{err}"))
        .unwrap_or_default();

    if read_only_analysis && has_sufficient_analysis_evidence(history) {
        format!(
            "⚠️ {reason}.{suffix}\n\n{}",
            synthesize_read_only_outcome(history)
        )
    } else {
        format!("⚠️ 任務已 fail-fast 中止：{reason}.{suffix}")
    }
}

fn derive_result_status(
    dry_run: bool,
    analysis_completed: bool,
    verified: bool,
    build_verified: bool,
    attempted_write: bool,
    files_modified_count: usize,
    had_tool_errors: bool,
) -> CodingResultStatus {
    if verified {
        return CodingResultStatus::Verified;
    }

    if analysis_completed {
        return if dry_run {
            CodingResultStatus::DryRunDone
        } else {
            CodingResultStatus::Done
        };
    }

    if dry_run && !had_tool_errors {
        return CodingResultStatus::DryRunDone;
    }

    if !build_verified
        || (attempted_write && files_modified_count == 0)
        || (had_tool_errors && files_modified_count == 0)
    {
        return CodingResultStatus::FollowupNeeded;
    }

    CodingResultStatus::Done
}

fn build_change_summary(
    files_modified: &[String],
    verified: bool,
    dry_run: bool,
    auto_committed: bool,
    outcome: &str,
) -> String {
    let mut parts = Vec::new();

    if files_modified.is_empty() {
        parts.push(if dry_run {
            "僅分析，未寫入檔案".to_string()
        } else {
            "未偵測到檔案變更".to_string()
        });
    } else {
        let listed = files_modified
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if files_modified.len() > 3 {
            parts.push(format!(
                "已變更 {} 個檔案：{} …",
                files_modified.len(),
                listed
            ));
        } else {
            parts.push(format!(
                "已變更 {} 個檔案：{}",
                files_modified.len(),
                listed
            ));
        }
    }

    if dry_run {
        parts.push("dry-run".to_string());
    } else if verified {
        parts.push("cargo check 通過".to_string());
    } else {
        parts.push("待人工確認".to_string());
    }

    if auto_committed {
        parts.push("已自動 commit".to_string());
    }

    let preview = preview_text(outcome);
    if !preview.is_empty() {
        parts.push(format!("摘要：{preview}"));
    }

    parts.join("｜")
}

// ── Public runner ─────────────────────────────────────────────────────────────

pub async fn run_coding_via_adk(
    task: String,
    dry_run: bool,
    tracker: Option<TaskTracker>,
    context_block: Option<String>,
) -> CodingAgentResponse {
    // Load recent conversation context (UI session, peer_id=None).
    let context_hint = load_recent_context(5, None)
        .ok()
        .filter(|v| !v.is_empty())
        .map(|entries| {
            entries
                .iter()
                .map(|e| format!("User: {}\nAssistant: {}", e.user_msg, e.assistant_reply))
                .collect::<Vec<_>>()
                .join("\n---\n")
        });

    let runtime = AgentRuntime::default();
    let base_ctx = if let Some(ref task_tracker) = tracker {
        runtime.context_with_tracker("coding_request", task_tracker.clone())
    } else {
        runtime.context("coding_request")
    };
    let ctx = base_ctx
        .with_optional_tracker(tracker)
        .with_context_hint(context_hint)
        .with_metadata("agent", "coding_agent");

    let input = json!(CodingRequest {
        task: task.clone(),
        max_iterations: None,
        dry_run,
        context_block
    });
    let response = match runtime.run(&CodingAgent, ctx, input).await {
        Ok(v) => serde_json::from_value(v).unwrap_or_else(|_| CodingAgentResponse {
            outcome: "Completed (response parse error)".to_string(),
            result_status: CodingResultStatus::Error,
            ..Default::default()
        }),
        Err(e) => {
            sirin_log!("[coding_agent] run failed: {e}");
            CodingAgentResponse {
                outcome: format!("Error: {e}"),
                result_status: CodingResultStatus::Error,
                ..Default::default()
            }
        }
    };

    crate::events::publish(crate::events::AgentEvent::CodingAgentCompleted {
        task: task.chars().take(80).collect(),
        success: response.result_status.is_success(),
        files_modified: response.files_modified.clone(),
    });

    response
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Live integration test — requires LM Studio running at http://localhost:1234.
    /// Run with: `cargo test -- --ignored live_coding`
    #[tokio::test]
    #[ignore = "requires local LM Studio at http://localhost:1234"]
    async fn live_coding_agent_reads_and_summarises_file() {
        // Load .env so shared_llm() picks up LM_STUDIO_* vars.
        let _ = dotenvy::dotenv();

        let response = run_coding_via_adk(
            "Read src/code_graph.rs and summarise what parse_rust_file does in 2-3 sentences. \
             Do NOT modify any files."
                .to_string(),
            true, // dry_run — no writes allowed
            None,
            None, // context_block
        )
        .await;

        println!("\n=== CodingAgent live test ===");
        println!("Outcome:    {}", response.outcome);
        println!("Iterations: {}", response.iterations_used);
        println!("dry_run:    {}", response.dry_run);
        println!("Files modified: {:?}", response.files_modified);
        for (i, step) in response.trace.iter().enumerate() {
            println!("Step {i}:\n{step}");
        }

        assert!(
            !response.outcome.starts_with("Error:"),
            "CodingAgent returned a hard error: {}",
            response.outcome
        );
        assert!(
            response.iterations_used > 0,
            "Agent must have taken at least one ReAct step"
        );
        assert!(!response.outcome.is_empty(), "Outcome must not be empty");
        // dry_run=true → agent must not write anything
        assert!(
            response.files_modified.is_empty(),
            "dry_run=true but files were modified: {:?}",
            response.files_modified
        );
    }

    #[test]
    fn extract_path_hints_from_task_finds_explicit_repo_paths() {
        let task = "請幫我檢查 src/telegram.rs 和 src/llm.rs，看看 task 開始時要在哪裡加 log。";
        let hints = extract_path_hints_from_task(task);
        assert!(
            hints.iter().any(|p| p == "src/telegram.rs"),
            "missing telegram path: {hints:?}"
        );
        assert!(
            hints.iter().any(|p| p == "src/llm.rs"),
            "missing llm path: {hints:?}"
        );
    }

    #[test]
    fn read_only_analysis_task_detection_is_reasonable() {
        assert!(is_read_only_analysis_task(
            "請分析 src/telegram.rs，不要寫入檔案"
        ));
        assert!(is_read_only_analysis_task(
            "Explain what this module does without modifying files."
        ));
        assert!(!is_read_only_analysis_task(
            "請修改 src/telegram/mod.rs 並加入新的 log"
        ));
    }

    #[test]
    fn salvage_non_json_final_answer_uses_grounded_paths() {
        let history = vec![
            HistoryEntry {
                thought: "read telegram mod".to_string(),
                action: "local_file_read".to_string(),
                action_input: json!({"path": "src/telegram/mod.rs"}),
                observation: "ok".to_string(),
                pinned: true,
            },
            HistoryEntry {
                thought: "read llm".to_string(),
                action: "local_file_read".to_string(),
                action_input: json!({"path": "src/llm.rs"}),
                observation: "ok".to_string(),
                pinned: true,
            },
        ];

        let summary = salvage_non_json_final_answer("", &history);
        assert!(
            summary.contains("src/telegram/mod.rs"),
            "summary should cite inspected paths: {summary}"
        );
        assert!(
            summary.contains("src/llm.rs"),
            "summary should cite inspected paths: {summary}"
        );
    }

    #[test]
    fn file_read_cache_key_distinguishes_line_ranges() {
        let full = file_read_cache_key(&json!({"path": "src/telegram/mod.rs"}));
        let ranged = file_read_cache_key(
            &json!({"path": "src/telegram/mod.rs", "start_line": 1, "end_line": 120}),
        );
        assert_ne!(
            full, ranged,
            "cache key should include line-range information"
        );
    }

    #[test]
    fn fail_fast_outcome_mentions_reason() {
        let history = vec![HistoryEntry {
            thought: "read telegram mod".to_string(),
            action: "local_file_read".to_string(),
            action_input: json!({"path": "src/telegram/mod.rs"}),
            observation: "ok".to_string(),
            pinned: true,
        }];

        let msg = build_fail_fast_outcome("連續多步沒有新進展", &history, Some("cache hit"), true);
        assert!(
            msg.contains("連續多步沒有新進展"),
            "reason should be preserved: {msg}"
        );
    }

    #[test]
    fn change_summary_mentions_files_and_verification() {
        let files = vec![
            "src/ui.rs".to_string(),
            "src/agents/coding_agent.rs".to_string(),
        ];
        let summary =
            build_change_summary(&files, true, false, true, "已更新 UI 與 task board 顯示");

        assert!(
            summary.contains("已變更 2 個檔案"),
            "file count should be included: {summary}"
        );
        assert!(
            summary.contains("cargo check 通過"),
            "verification result should be included: {summary}"
        );
        assert!(
            summary.contains("已自動 commit"),
            "auto-commit should be included: {summary}"
        );
    }

    #[test]
    fn derive_result_status_treats_dry_run_analysis_as_done() {
        let status = derive_result_status(true, true, false, false, false, 0, true);
        assert_eq!(status, CodingResultStatus::DryRunDone);
    }

    #[test]
    fn derive_result_status_marks_unverified_write_as_followup() {
        let status = derive_result_status(false, false, false, false, true, 0, true);
        assert_eq!(status, CodingResultStatus::FollowupNeeded);
    }

    /// Live dry-run development-task test using the real coding workflow.
    /// Run: `cargo test gemini_dry_run_real_dev_task -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "requires Gemini API key in .env (LLM_PROVIDER=gemini)"]
    async fn gemini_dry_run_real_dev_task() {
        let _ = dotenvy::dotenv();

        let task = "請分析 `src/telegram.rs` 的 listener / 回覆流程，找出實際應修改的檔案，並說明若要在任務開始時印出 AI backend 與 model，最小修改點會在哪裡。不要寫入檔案。";

        let response = run_coding_via_adk(task.to_string(), true, None, None).await;

        println!("\n=== Gemini dry-run dev task ===");
        println!("Outcome: {}", response.outcome);
        println!("Iterations: {}", response.iterations_used);
        println!("Trace:\n{}", response.trace.join("\n---\n"));

        assert!(
            !response.outcome.starts_with("Error:"),
            "coding workflow returned an error: {}",
            response.outcome
        );
        assert!(
            response.iterations_used > 0,
            "expected the coding workflow to take at least one step"
        );
        assert!(!response.outcome.is_empty(), "outcome should not be empty");
        assert!(
            !response.verified,
            "dry-run task should not be marked as build-verified"
        );
        let normalized = response.outcome.to_lowercase();
        assert!(
            normalized.contains("src/telegram/mod.rs")
                || normalized.contains("src/telegram/reply.rs")
                || normalized.contains("backend")
                || normalized.contains("model"),
            "expected grounded analysis details in outcome, got: {}",
            response.outcome
        );
    }

    /// Gemini 能力驗證：設計並新增兩個模組（AppConfig + LogManager）
    /// Run: cargo test gemini_config_and_log -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires Gemini API key in .env (LLM_PROVIDER=gemini)"]
    async fn gemini_config_and_log() {
        let _ = dotenvy::dotenv();

        let task = "分析 Sirin 專案目前的配置管理（.env 各模組 from_env）和日誌系統（src/log_buffer.rs），\
            然後新增以下兩個模組：\
            \n1. src/config.rs — AppConfig struct，統一管理 LLM / Telegram / Followup 的配置項，\
            提供 AppConfig::load() 從環境變數讀取，並加 #[cfg(test)] 單元測試。\
            \n2. src/log_manager.rs — LogLevel enum (Error/Warn/Info/Debug)、\
            一個 filtered_recent(level, n) 函數按等級過濾 log_buffer 內容，\
            以及 export_to_string(n) 匯出最近 n 條為純文字，並加 #[cfg(test)] 單元測試。\
            \n不要修改任何現有檔案。兩個新檔案都要能通過 cargo check。";

        let response = run_coding_via_adk(
            task.to_string(),
            false, // actually write the files
            None,
            None,
        )
        .await;

        println!("\n======== Gemini Coding Agent ========");
        println!("Iterations used : {}", response.iterations_used);
        println!("Files modified  : {:?}", response.files_modified);
        println!("cargo check pass: {}", response.verified);
        println!("Outcome:\n{}", response.outcome);
        if let Some(ref diff) = response.diff {
            println!(
                "\n--- diff preview ---\n{}",
                diff.chars().take(1200).collect::<String>()
            );
        }

        assert!(
            !response.outcome.starts_with("Error:"),
            "agent error: {}",
            response.outcome
        );
        assert!(!response.files_modified.is_empty(), "no files were written");
        assert!(response.verified, "cargo check failed after changes");
    }

    #[test]
    fn parse_react_step_valid_json() {
        let raw = r#"{"thought":"read the file","action":"local_file_read","action_input":{"path":"src/main.rs"}}"#;
        let step = parse_react_step(raw);
        assert_eq!(step.action, "local_file_read");
        assert!(step.final_answer.is_none());
    }

    #[test]
    fn parse_react_step_done() {
        let raw =
            r#"{"thought":"done","action":"DONE","action_input":{},"final_answer":"Applied fix."}"#;
        let step = parse_react_step(raw);
        assert_eq!(step.action, "DONE");
        assert_eq!(step.final_answer.as_deref(), Some("Applied fix."));
    }

    #[test]
    fn parse_react_step_bad_json_requests_retry() {
        let step = parse_react_step("not valid json at all");
        assert_eq!(step.action, "INVALID_JSON");
        assert!(step.parse_error);
        assert!(step.final_answer.is_none());
    }

    #[test]
    fn overall_verified_requires_actual_write_evidence_after_write_attempts() {
        assert!(!overall_verified(false, true, true, 0, true));
        assert!(overall_verified(false, true, true, 1, true));
        assert!(overall_verified(false, true, false, 0, false));
    }

    #[test]
    fn parse_react_step_strips_markdown_fences() {
        let raw = "```json\n{\"thought\":\"t\",\"action\":\"DONE\",\"action_input\":{}}\n```";
        let step = parse_react_step(raw);
        assert_eq!(step.action, "DONE");
    }

    /// LLM 在最後幾個 iter 常在 JSON 前加中文說明句，例如：
    /// "我已完成所有步驟：\n{ ... }"
    /// 這個測試確保 extract_json_body 能正確切出 JSON。
    #[test]
    fn parse_react_step_json_with_preamble_text() {
        let raw = "我已經成功完成了所有步驟：\n\
                   {\"thought\":\"done\",\"action\":\"DONE\",\"action_input\":{},\
                   \"final_answer\":\"分析完成：Sirin 是一個純 Rust AI Agent。\"}";
        let step = parse_react_step(raw);
        assert_eq!(step.action, "DONE");
        assert!(
            step.final_answer.as_deref().unwrap_or("").contains("Sirin"),
            "final_answer should contain the summary, got: {:?}",
            step.final_answer
        );
    }

    /// JSON 後面附有後置文字時也應正確解析。
    #[test]
    fn parse_react_step_json_with_postamble_text() {
        let raw = "{\"thought\":\"t\",\"action\":\"local_file_read\",\
                   \"action_input\":{\"path\":\"src/main.rs\"}}\n以上是我的回應。";
        let step = parse_react_step(raw);
        assert_eq!(step.action, "local_file_read");
    }

    #[test]
    fn truncate_to_bytes_ascii() {
        assert_eq!(truncate_to_bytes("hello world", 5), "hello");
        assert_eq!(truncate_to_bytes("hi", 10), "hi");
        assert_eq!(truncate_to_bytes("", 10), "");
    }

    #[test]
    fn truncate_to_bytes_cjk_boundary() {
        // "你好世界" = 4 chars × 3 bytes = 12 bytes total.
        // Truncating at 9 bytes should give "你好世" (9 bytes), not corrupt mid-char.
        let s = "你好世界";
        assert_eq!(s.len(), 12);
        let truncated = truncate_to_bytes(s, 9);
        assert_eq!(truncated, "你好世");
        // Truncating at 10 bytes (not a char boundary) must still yield "你好世".
        let truncated2 = truncate_to_bytes(s, 10);
        assert_eq!(truncated2, "你好世");
    }

    #[test]
    fn auto_commit_summary_fits_in_72_bytes() {
        // A long CJK task string must not produce a summary > 72 bytes.
        let long_task: String =
            "幫我優化這個專案的效能，讓它更快更穩定，不要動到測試檔案".repeat(5);
        let prefix = "chore(sirin-agent): ";
        let max_summary_bytes = 72usize.saturating_sub(prefix.len());
        let summary = truncate_to_bytes(long_task.trim(), max_summary_bytes);
        assert!(
            summary.len() <= max_summary_bytes,
            "summary too long: {} bytes",
            summary.len()
        );
        // Must be valid UTF-8 (no panics from str operations).
        let _ = format!("{prefix}{summary}");
    }
}
