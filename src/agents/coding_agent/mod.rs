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

// The ReAct loop helpers are called via dynamic ADK dispatch (runtime.run(&CodingAgent, …));
// Rust's static analysis cannot trace through the trait object, so suppress dead_code warnings.
#![allow(dead_code)]

mod finalize;
mod helpers;
mod prompt;
mod react;
mod rollback;
mod state;
mod step;
mod verdict;
mod verify;

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::memory::load_recent_context;
use crate::persona::{CodingAgentConfig, Persona, TaskTracker};
use crate::sirin_log;

use helpers::{is_read_only_analysis_task, preview_text};
use prompt::{gather_project_context, make_plan};
use state::RunState;
use verify::VerifyOutcome;

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
    ) -> futures_util::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: CodingRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid coding request payload: {e}"))?;

            let config = Persona::cached().map(|p| p.coding_agent).unwrap_or_default();

            if !config.enabled {
                return Err("Coding agent is disabled in persona config.".to_string());
            }

            let response = run_react_loop(ctx, &request, &config).await?;
            serde_json::to_value(response).map_err(|e| e.to_string())
        }
        .boxed()
    }
}

// ── ReAct loop orchestration ─────────────────────────────────────────────────
//
// Phase 1: gather project context (overview + codebase search + recent
//          conversation + router-injected memory).
// Phase 2: one-shot LLM call that produces the numbered plan.
// Phase 2.5: record the baseline commit so rollback has an anchor.
// Phase 3: main ReAct loop — delegated to [`react::run_react_iterations`].
// Phase 4: verify + auto-fix — delegated to [`verify::run_verify_and_autofix`];
//          may short-circuit with a `Rollback` response.
// Phase 5: diff + auto-commit + outcome synthesis — [`finalize::finalize`].

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

    // ── Phase 1: gather project context ──────────────────────────────────────
    let mut project_ctx = gather_project_context(ctx, &request.task).await;

    // Append recent conversation context if available so the agent has
    // awareness of what the user was just discussing.
    if let Some(hint) = ctx.context_hint() {
        project_ctx.push_str("\n\n## Recent conversation context\n");
        project_ctx.push_str(hint);
    }

    // Append router-injected memory context when provided.
    if let Some(block) = &request.context_block {
        if !block.trim().is_empty() {
            project_ctx.push_str("\n\n## Conversation memory (router-injected)\n");
            project_ctx.push_str(block);
        }
    }

    // ── Phase 2: planning call ───────────────────────────────────────────────
    let plan = make_plan(ctx, &request.task, &project_ctx).await;
    sirin_log!("[coding_agent] plan ready ({} chars)", plan.len());

    // ── Phase 2.5: record baseline commit (rollback anchor) ──────────────────
    // Call git directly — bypasses the shell_exec allowlist which only permits
    // cargo commands. Never panics; baseline is simply None if git is absent.
    let baseline_commit = if !dry_run {
        use crate::platform::NoWindow;
        std::process::Command::new("git")
            .no_window()
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

    // ── Phase 3: ReAct loop ──────────────────────────────────────────────────
    let mut state = RunState::default();
    react::run_react_iterations(
        ctx,
        request,
        config,
        &project_ctx,
        &plan,
        dry_run,
        read_only_analysis,
        max_iter,
        &mut state,
    )
    .await?;

    // ── Phase 4: verification + auto-fix (may early-return Rollback) ─────────
    let (build_verified, verification_output) = match verify::run_verify_and_autofix(
        ctx,
        &project_ctx,
        config,
        baseline_commit.as_deref(),
        dry_run,
        &mut state,
    )
    .await
    {
        VerifyOutcome::Verified {
            build_verified,
            verification_output,
        } => (build_verified, verification_output),
        VerifyOutcome::RolledBack(response) => return Ok(response),
    };

    // ── Phase 5: finalize (diff + auto-commit + response) ────────────────────
    Ok(finalize::finalize(
        ctx,
        &request.task,
        &state.history,
        state.files_modified,
        state.final_answer,
        read_only_analysis,
        dry_run,
        build_verified,
        state.attempted_write,
        state.had_tool_errors,
        state.last_tool_error,
        verification_output,
    )
    .await)
}

// ── Public runner ─────────────────────────────────────────────────────────────

pub async fn run_coding_via_adk(
    task: String,
    dry_run: bool,
    tracker: Option<TaskTracker>,
    context_block: Option<String>,
) -> CodingAgentResponse {
    // Load recent conversation context (UI session, peer_id=None).
    let context_hint = load_recent_context(5, None, None)
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
    use super::helpers::{
        extract_path_hints_from_task, file_read_cache_key, truncate_to_bytes,
    };
    use super::prompt::parse_react_step;
    use super::verdict::{
        build_change_summary, build_fail_fast_outcome, derive_result_status, overall_verified,
        salvage_non_json_final_answer, HistoryEntry,
    };
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
