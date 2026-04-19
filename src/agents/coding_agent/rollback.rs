//! Baseline rollback — invoked when `cargo check` still fails after the
//! auto-fix ReAct iterations.
//!
//! Restores only the files this task touched back to their state at the
//! recorded baseline commit; files outside the task's scope are left alone.
//! Files that didn't exist at the baseline are deleted.

use crate::adk::AgentContext;
use crate::sirin_log;

use super::{CodingAgentResponse, CodingResultStatus};

/// Perform a baseline rollback and build the terminal `Rollback` response.
///
/// Uses `git show {commit}:{path}` (no allowlist needed) to fetch baseline
/// contents. For files that didn't exist at baseline, deletes them. Records
/// a system event summarising the rollback result.
pub(super) fn perform_rollback(
    ctx: &AgentContext,
    commit: &str,
    files_modified: &[String],
    iterations_used: usize,
    verification_output: Option<String>,
    dry_run: bool,
) -> CodingAgentResponse {
    sirin_log!(
        "[coding_agent] ROLLBACK: restoring {} file(s) to {commit}",
        files_modified.len()
    );

    let mut rolled_back = Vec::new();
    let mut rollback_errors = Vec::new();

    use crate::platform::NoWindow;
    for path in files_modified {
        let result = std::process::Command::new("git")
            .no_window()
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
                if stderr.contains("does not exist") || stderr.contains("exists on disk") {
                    let _ = std::fs::remove_file(path);
                    rolled_back.push(path.clone());
                } else {
                    rollback_errors.push(format!("{path}: git show failed ({})", stderr.trim()));
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

    CodingAgentResponse {
        outcome: rollback_msg,
        result_status: CodingResultStatus::Rollback,
        change_summary: "已自動回滾本次修改，請重新確認任務描述後再試。".to_string(),
        files_modified: vec![],
        iterations_used,
        diff: None,
        verified: false,
        verification_output,
        trace: vec![],
        dry_run,
    }
}
