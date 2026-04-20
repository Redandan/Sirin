//! 持久化的 Claude Code session。
//!
//! 每個角色（PM / Engineer / Tester）擁有一個 `PersistentSession`。
//! 第一次 `send()` 時建立新 session 並記錄 session_id；
//! 之後每次 `send()` 都加 `--continue`，讓對話在同一個 session 延續。
//!
//! session_id 存在 `data/multi_agent/<role>.json`，重啟 Sirin 後仍可繼續。
//! 使用者可在自己的 terminal 執行 `claude --resume <session_id>` 查看對話歷史。

use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use crate::platform;
use super::roles::DRY_RUN_ADDENDUM;

// ── 持久化資料 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SessionFile {
    session_id: Option<String>,
    role:       String,
    started_at: String,
    turns:      u32,
}

// ── PersistentSession ─────────────────────────────────────────────────────────

pub struct PersistentSession {
    pub role:          String,
    pub cwd:           String,
    pub system_prompt: String,
    pub worker_id:     usize,    // T1-1: per-worker session isolation
    /// Project key — namespaces session files when running cross-repo tasks.
    /// Empty string OR "sirin" → legacy file naming (`{role}.json` / `w{N}_{role}.json`).
    /// Anything else → adds a `p{key}_` prefix so each project keeps its own
    /// session_id / turn-count / claude --continue history.
    pub project_key:   String,
    /// Per-task tool extensions appended to the role's static whitelist.
    /// Set/cleared by `AgentTeam::set_extra_tools()` for the duration of one
    /// task; not persisted (resets to empty on every reload).
    pub extra_tools:   Vec<String>,
    /// Per-task dry-run flag — see `queue::ProjectContext::dry_run`.
    /// When `true`, `send()` prepends a do-not-mutate-external-state
    /// addendum to the message (the hard stop on auto-commenting lives in
    /// `worker.rs`). Not persisted; reset to `false` on every reload.
    pub dry_run:       bool,
    state:             SessionFile,
}

impl PersistentSession {
    /// 從磁碟載入（或建立新的）session 狀態（worker 0，向後相容）。
    pub fn load(role: &str, cwd: &str, system_prompt: &str) -> Self {
        Self::load_for_worker(role, 0, cwd, system_prompt)
    }

    /// 從磁碟載入指定 worker 的 session 狀態。
    ///
    /// `worker_id == 0` 走原有路徑 `data/multi_agent/{role}.json`（向後相容）。
    /// `worker_id >= 1` 走 `data/multi_agent/w{worker_id}_{role}.json`。
    pub fn load_for_worker(role: &str, worker_id: usize, cwd: &str, system_prompt: &str) -> Self {
        Self::load_for_worker_project(role, worker_id, "", cwd, system_prompt)
    }

    /// 從磁碟載入指定 (worker, project) 的 session 狀態。
    ///
    /// `project_key` 為空字串或 "sirin" 時走 legacy file naming，與
    /// `load_for_worker` 結果完全相同 — 確保升級不破壞既有 PM/Engineer
    /// session 歷史。其他值 (e.g. "agora_market") 會加 `p{key}_` 命名空間。
    pub fn load_for_worker_project(
        role: &str,
        worker_id: usize,
        project_key: &str,
        cwd: &str,
        system_prompt: &str,
    ) -> Self {
        let path = state_path_for_project(role, worker_id, project_key);
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<SessionFile>(&s).ok())
            .unwrap_or_else(|| SessionFile {
                role: role.to_string(), ..Default::default()
            });
        Self {
            role:          role.to_string(),
            cwd:           cwd.to_string(),
            system_prompt: system_prompt.to_string(),
            worker_id,
            project_key:   project_key.to_string(),
            extra_tools:   Vec::new(),
            dry_run:       false,
            state,
        }
    }

    /// 送一條訊息給這個 session，回傳助理的回覆。
    /// 第一次呼叫時會在訊息前加上 system_prompt。
    pub fn send(&mut self, message: &str) -> Result<String, String> {
        let is_new   = self.state.session_id.is_none();

        // Dry-run addendum is injected into EVERY message (not just first turn)
        // because Claude --continue may forget the original system prompt under
        // long context — repeating the rule is cheap and keeps the guardrail
        // sticky across all 5 assign_task iterations.
        let body = if self.dry_run {
            format!("{}\n\n{}", DRY_RUN_ADDENDUM, message)
        } else {
            message.to_string()
        };

        let prompt = if is_new {
            format!("{}\n\n---\n\n{}", self.system_prompt.trim(), body)
        } else {
            body
        };

        // Build merged whitelist: role's static base + task-supplied extras.
        // Unknown roles → empty Vec → None → god mode (legacy fallback).
        let merged = crate::multi_agent::roles::merged_whitelist_for(
            &self.role, &self.extra_tools,
        );
        let merged_refs: Vec<&str> = merged.iter().map(|s| s.as_str()).collect();
        let whitelist: Option<&[&str]> = if merged.is_empty() {
            None
        } else {
            Some(merged_refs.as_slice())
        };

        let (output, session_id) = crate::claude_session::run_one_turn_scoped(
            &self.cwd,
            &prompt,
            !is_new,   // continuation = true when session already exists
            whitelist,
        )?;

        // 第一次才存 session_id（--continue 不會改 id）
        if is_new && !session_id.is_empty() {
            self.state.session_id  = Some(session_id);
            self.state.role        = self.role.clone();
            self.state.started_at  = chrono::Local::now().to_rfc3339();
        }
        self.state.turns += 1;
        self.save();

        Ok(output)
    }

    /// session_id，讓使用者可以 `claude --resume <id>` 查看對話歷史。
    pub fn session_id(&self) -> Option<&str> {
        self.state.session_id.as_deref()
    }

    pub fn turns(&self) -> u32 { self.state.turns }

    /// 重置 session（開始全新對話）。
    pub fn reset(&mut self) {
        self.state = SessionFile {
            role: self.role.clone(), ..Default::default()
        };
        let _ = std::fs::remove_file(state_path_for_project(
            &self.role, self.worker_id, &self.project_key,
        ));
    }

    fn save(&self) {
        let path = state_path_for_project(&self.role, self.worker_id, &self.project_key);
        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Ok(json) = serde_json::to_string_pretty(&self.state) {
            let _ = std::fs::write(&path, json);
        }
    }
}

/// State file path for a (role, worker_id) pair (legacy 2-arg form).
/// Kept for any external callers that don't yet know about projects.
#[allow(dead_code)]
fn state_path_for(role: &str, worker_id: usize) -> PathBuf {
    state_path_for_project(role, worker_id, "")
}

/// State file path for a (role, worker_id, project_key) triple.
///
/// Backwards-compat rules so existing pm.json / engineer.json / tester.json
/// keep working without manual migration:
///   - project_key empty OR "sirin" → no `p{key}_` prefix
///   - worker_id 0                  → no `w{N}_` prefix
///
/// Examples:
///   ("pm", 0, "")                → `pm.json`              (legacy w0 Sirin)
///   ("pm", 1, "")                → `w1_pm.json`           (legacy w1 Sirin)
///   ("pm", 0, "agora_market")    → `pagora_market_pm.json`
///   ("pm", 2, "agora_market")    → `w2_pagora_market_pm.json`
fn state_path_for_project(role: &str, worker_id: usize, project_key: &str) -> PathBuf {
    let proj = project_key.trim();
    let proj_prefix = if proj.is_empty() || proj.eq_ignore_ascii_case("sirin") {
        String::new()
    } else {
        format!("p{proj}_")
    };
    let filename = if worker_id == 0 {
        format!("{proj_prefix}{role}.json")
    } else {
        format!("w{worker_id}_{proj_prefix}{role}.json")
    };
    platform::app_data_dir()
        .join("data")
        .join("multi_agent")
        .join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_paths_preserved() {
        // Worker 0 + Sirin (or empty) → original pm.json
        assert_eq!(
            state_path_for_project("pm", 0, "").file_name().unwrap(),
            "pm.json"
        );
        assert_eq!(
            state_path_for_project("pm", 0, "sirin").file_name().unwrap(),
            "pm.json"
        );
        assert_eq!(
            state_path_for_project("engineer", 1, "").file_name().unwrap(),
            "w1_engineer.json"
        );
    }

    #[test]
    fn project_paths_namespaced() {
        assert_eq!(
            state_path_for_project("pm", 0, "agora_market").file_name().unwrap(),
            "pagora_market_pm.json"
        );
        assert_eq!(
            state_path_for_project("engineer", 2, "agora_market").file_name().unwrap(),
            "w2_pagora_market_engineer.json"
        );
    }

    #[test]
    fn project_key_case_insensitive_for_sirin() {
        assert_eq!(
            state_path_for_project("pm", 0, "Sirin").file_name().unwrap(),
            "pm.json"
        );
        assert_eq!(
            state_path_for_project("pm", 0, "SIRIN").file_name().unwrap(),
            "pm.json"
        );
    }
}
