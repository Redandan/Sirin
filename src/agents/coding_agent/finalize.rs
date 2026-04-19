//! Terminal phase of a coding agent run — after ReAct + verify finish, this
//! module decides overall success, produces a human-readable outcome, records
//! the system event, and assembles the final [`CodingAgentResponse`].
//!
//! Also wraps side effects the finalize phase triggers: `file_diff` (captures
//! the working-tree delta for the UI) and `auto_commit` (stages + commits only
//! when `cargo check` passed).

use serde_json::{json, Value};

use crate::adk::AgentContext;
use crate::sirin_log;

use super::helpers::{preview_text, preview_tool_input, truncate_to_bytes};
use super::verdict::{
    build_change_summary, derive_result_status, followup_reason, has_sufficient_analysis_evidence,
    overall_verified, synthesize_read_only_outcome, HistoryEntry,
};
use super::CodingAgentResponse;

/// Assemble the terminal response from the accumulated run state.
///
/// Runs `file_diff` when not in dry-run mode, auto-commits when the build
/// passed and at least one file changed, then synthesises the outcome text,
/// change summary, and result status.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize(
    ctx: &AgentContext,
    task: &str,
    history: &[HistoryEntry],
    files_modified: Vec<String>,
    final_answer: String,
    read_only_analysis: bool,
    dry_run: bool,
    build_verified: bool,
    attempted_write: bool,
    had_tool_errors: bool,
    last_tool_error: Option<String>,
    verification_output: Option<String>,
) -> CodingAgentResponse {
    let verified = overall_verified(
        dry_run,
        build_verified,
        attempted_write,
        files_modified.len(),
        had_tool_errors,
    );

    // ── Step 5: diff ──────────────────────────────────────────────────────────
    let diff = if !dry_run { get_diff(ctx).await } else { None };

    // ── Step 6: auto-commit when verified ─────────────────────────────────────
    // Only commit when: not dry_run, cargo check passed, files were actually
    // changed, and git is available.
    let auto_committed = if !dry_run && verified && !files_modified.is_empty() {
        auto_commit(task, &files_modified).await
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
        if read_only_analysis && has_sufficient_analysis_evidence(history) {
            synthesize_read_only_outcome(history)
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

    if auto_committed {
        outcome = format!("{outcome}\n\n✅ 已自動 commit（cargo check 通過）");
    }

    let change_summary =
        build_change_summary(&files_modified, verified, dry_run, auto_committed, &outcome);
    let analysis_completed = read_only_analysis && has_sufficient_analysis_evidence(history);
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

    CodingAgentResponse {
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
    }
}

// ── Local helpers (kept private — only finalize() uses them) ─────────────────

async fn get_diff(ctx: &AgentContext) -> Option<String> {
    ctx.call_tool("file_diff", json!({}))
        .await
        .ok()
        .and_then(|v| v.get("diff").and_then(Value::as_str).map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
}

/// Stage only the files this task modified and create a commit.
/// Returns true on success. Never panics — all errors are logged and ignored.
async fn auto_commit(task: &str, files_modified: &[String]) -> bool {
    use crate::platform::NoWindow;
    // Stage only the modified files (never `git add -A`).
    let add_ok = std::process::Command::new("git")
        .no_window()
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
        .no_window()
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
