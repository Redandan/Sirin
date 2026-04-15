//! Outcome synthesis for the coding agent.
//!
//! Shared types and post-processing logic used by the ReAct loop to decide
//! whether a task succeeded, why follow-up is needed, and how to summarise
//! the result for the UI / task board.

#![allow(dead_code)]

use serde_json::Value;

use super::helpers::preview_text;
use super::CodingResultStatus;

// ── History entry ─────────────────────────────────────────────────────────────

pub(super) struct HistoryEntry {
    pub(super) thought: String,
    pub(super) action: String,
    pub(super) action_input: Value,
    pub(super) observation: String,
    /// Pinned entries (e.g. file_read) are always included in the history
    /// window regardless of how many iterations have passed.
    pub(super) pinned: bool,
}

pub(super) fn has_sufficient_analysis_evidence(history: &[HistoryEntry]) -> bool {
    history
        .iter()
        .filter(|h| h.action == "local_file_read" && !h.observation.starts_with("ERROR:"))
        .count()
        >= 2
}

pub(super) fn inspected_paths_from_history(history: &[HistoryEntry]) -> Vec<String> {
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

// ── Verification verdicts ─────────────────────────────────────────────────────

pub(super) fn overall_verified(
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

pub(super) fn followup_reason(
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

pub(super) fn derive_result_status(
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

// ── Outcome narrative ─────────────────────────────────────────────────────────

pub(super) fn salvage_non_json_final_answer(raw: &str, history: &[HistoryEntry]) -> String {
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

pub(super) fn synthesize_read_only_outcome(history: &[HistoryEntry]) -> String {
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

pub(super) fn build_fail_fast_outcome(
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

pub(super) fn build_change_summary(
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
