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

// ── Tests ────────────────────────────────────────────────────────────────────
//
// Issue #252 — coverage for the coding-agent verdict logic. Pure functions
// throughout — no LLM, no FS, no async. Each test pins one truth-table row
// of the decision matrix.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_entry(path: &str) -> HistoryEntry {
        HistoryEntry {
            thought:      String::new(),
            action:       "local_file_read".into(),
            action_input: json!({ "path": path }),
            observation:  "// file contents".into(),
            pinned:       false,
        }
    }

    fn err_entry() -> HistoryEntry {
        HistoryEntry {
            thought:      String::new(),
            action:       "local_file_read".into(),
            action_input: json!({ "path": "x.rs" }),
            observation:  "ERROR: not found".into(),
            pinned:       false,
        }
    }

    // ── has_sufficient_analysis_evidence ────────────────────────────────────

    #[test]
    fn evidence_requires_two_successful_reads() {
        assert!(!has_sufficient_analysis_evidence(&[]));
        assert!(!has_sufficient_analysis_evidence(&[read_entry("a.rs")]));
        assert!(has_sufficient_analysis_evidence(&[
            read_entry("a.rs"), read_entry("b.rs"),
        ]));
        // Errored reads don't count.
        assert!(!has_sufficient_analysis_evidence(&[
            read_entry("a.rs"), err_entry(),
        ]));
    }

    #[test]
    fn inspected_paths_dedupes_and_skips_empty() {
        let h = vec![
            read_entry("src/foo.rs"),
            read_entry("src/foo.rs"), // duplicate
            read_entry("src/bar.rs"),
            read_entry(""),           // empty path skipped
        ];
        let paths = inspected_paths_from_history(&h);
        assert_eq!(paths, vec!["src/foo.rs", "src/bar.rs"]);
    }

    // ── overall_verified — full truth table ────────────────────────────────

    #[test]
    fn overall_verified_blocks_dry_run() {
        // dry_run always blocks the verified=true outcome.
        assert!(!overall_verified(true,  true,  false, 0, false));
        assert!(!overall_verified(true,  true,  true,  3, false));
    }

    #[test]
    fn overall_verified_blocks_when_build_fails() {
        // build_verified=false always blocks.
        assert!(!overall_verified(false, false, true, 3, false));
    }

    #[test]
    fn overall_verified_blocks_when_attempted_write_but_zero_files() {
        // Agent tried to write but no file actually changed → not verified.
        assert!(!overall_verified(false, true, true, 0, false));
    }

    #[test]
    fn overall_verified_blocks_when_tool_errors_and_zero_files() {
        // Tool errors with no successful writes → not verified.
        assert!(!overall_verified(false, true, false, 0, true));
    }

    #[test]
    fn overall_verified_passes_clean_run() {
        // build OK, files modified, no tool errors → verified.
        assert!(overall_verified(false, true, true, 2, false));
        // build OK, no write attempted (read-only analysis) → verified.
        assert!(overall_verified(false, true, false, 0, false));
        // tool errors but files were actually written → still verified.
        assert!(overall_verified(false, true, true, 2, true));
    }

    // ── followup_reason ─────────────────────────────────────────────────────

    #[test]
    fn followup_reason_silent_for_dry_run() {
        // dry_run → never produces a followup reason regardless of state.
        assert!(followup_reason(true, false, true, 0, true, Some("e")).is_none());
    }

    #[test]
    fn followup_reason_when_build_fails() {
        let r = followup_reason(false, false, false, 0, false, None);
        assert!(r.unwrap().contains("cargo check"));
    }

    #[test]
    fn followup_reason_when_attempted_write_but_zero_files_includes_last_error() {
        let r = followup_reason(false, true, true, 0, false, Some("patch failed"));
        let msg = r.unwrap();
        assert!(msg.contains("沒有任何檔案真正寫入"));
        assert!(msg.contains("patch failed"));
    }

    #[test]
    fn followup_reason_silent_when_clean() {
        let r = followup_reason(false, true, true, 2, false, None);
        assert!(r.is_none());
    }

    // ── derive_result_status — status precedence ────────────────────────────

    #[test]
    fn status_verified_wins_when_verified_true() {
        // verified=true short-circuits to Verified, ignoring everything else.
        let s = derive_result_status(true, true, true, false, false, 0, true);
        assert_eq!(s, super::super::CodingResultStatus::Verified);
    }

    #[test]
    fn status_dry_run_done_for_completed_analysis_in_dry_run() {
        let s = derive_result_status(true, true, false, true, false, 0, false);
        assert_eq!(s, super::super::CodingResultStatus::DryRunDone);
    }

    #[test]
    fn status_done_for_completed_analysis_outside_dry_run() {
        let s = derive_result_status(false, true, false, true, false, 0, false);
        assert_eq!(s, super::super::CodingResultStatus::Done);
    }

    #[test]
    fn status_followup_when_build_fails() {
        let s = derive_result_status(false, false, false, false, false, 0, false);
        assert_eq!(s, super::super::CodingResultStatus::FollowupNeeded);
    }

    #[test]
    fn status_followup_when_attempted_write_but_zero_files() {
        let s = derive_result_status(false, false, false, true, true, 0, false);
        assert_eq!(s, super::super::CodingResultStatus::FollowupNeeded);
    }

    // ── build_change_summary ────────────────────────────────────────────────

    #[test]
    fn change_summary_handles_no_files_dry_run() {
        let s = build_change_summary(&[], false, true, false, "");
        assert!(s.contains("僅分析"));
        assert!(s.contains("dry-run"));
    }

    #[test]
    fn change_summary_handles_no_files_real_run() {
        let s = build_change_summary(&[], false, false, false, "");
        assert!(s.contains("未偵測到檔案變更"));
        assert!(s.contains("待人工確認"));
    }

    #[test]
    fn change_summary_lists_first_three_files_and_truncates() {
        let files = vec![
            "a.rs".to_string(), "b.rs".into(), "c.rs".into(),
            "d.rs".into(), "e.rs".into(),
        ];
        let s = build_change_summary(&files, true, false, false, "outcome text");
        assert!(s.contains("已變更 5 個檔案"));
        assert!(s.contains("a.rs"));
        assert!(s.contains("c.rs"));
        // 4th + 5th truncated
        assert!(!s.contains("d.rs"));
        assert!(s.contains("…"));
        assert!(s.contains("cargo check 通過"));
    }

    #[test]
    fn change_summary_marks_auto_committed() {
        let s = build_change_summary(&["a.rs".into()], true, false, true, "");
        assert!(s.contains("已自動 commit"));
    }

    // ── salvage / synthesize narratives ─────────────────────────────────────

    #[test]
    fn salvage_short_raw_falls_back_to_evidence_summary() {
        // raw too short (< 40 chars) → use inspected_paths_from_history.
        let h = vec![read_entry("src/a.rs"), read_entry("src/b.rs")];
        let out = salvage_non_json_final_answer("ok", &h);
        assert!(out.contains("src/a.rs"));
        assert!(out.contains("src/b.rs"));
    }

    #[test]
    fn salvage_long_raw_passes_through() {
        let raw = "x".repeat(80);
        let out = salvage_non_json_final_answer(&raw, &[]);
        assert_eq!(out.trim(), raw);
    }

    #[test]
    fn synthesize_read_only_handles_no_history() {
        let out = synthesize_read_only_outcome(&[]);
        assert!(out.contains("分析完成"));
        assert!(out.contains("沒有寫入任何檔案"));
    }
}
