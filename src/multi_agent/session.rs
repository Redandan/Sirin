//! 持久化的 Agent session（Gemini API + 手動歷史管理）。
//!
//! 每個角色（PM / Engineer / Tester）擁有一個 `PersistentSession`。
//! 對話歷史完整保存在記憶體和磁碟，每次 `send()` 都帶完整 context。
//!
//! 優勢：
//! - Gemini 1M context window 可容納更長對話
//! - 不依賴 claude CLI，純 API 呼叫
//! - 成本降低 40 倍（Gemini vs Claude）
//!
//! session_id 存在 `data/multi_agent/<role>.json`，重啟 Sirin 後對話歷史仍可還原。

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
    /// 完整對話歷史（持久化到磁碟）
    /// 格式：[(role, content), (role, content), ...]
    /// role: "system" | "user" | "assistant"
    #[serde(default)]
    history:    Vec<(String, String)>,
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
    /// 使用 Gemini API 並手動管理對話歷史。
    pub fn send(&mut self, message: &str) -> Result<String, String> {
        let is_new = self.state.session_id.is_none();

        // Dry-run addendum 注入每次訊息
        let body = if self.dry_run {
            format!("{}\n\n{}", DRY_RUN_ADDENDUM, message)
        } else {
            message.to_string()
        };

        // 建構完整對話歷史
        let mut messages = Vec::new();

        // 第一次：加 system prompt
        if self.state.history.is_empty() {
            messages.push(crate::llm::LlmMessage::system(&self.system_prompt));
        }

        // 載入歷史對話
        for (role, content) in &self.state.history {
            let msg = match role.as_str() {
                "system" => crate::llm::LlmMessage::system(content),
                "user" => crate::llm::LlmMessage::user(content),
                _ => crate::llm::LlmMessage::assistant(content),
            };
            messages.push(msg);
        }

        // 加入當前訊息
        messages.push(crate::llm::LlmMessage::user(&body));

        // Context window 管理：超過 500K chars 時修剪歷史
        const MAX_HISTORY_CHARS: usize = 500_000; // Gemini 1M tokens ≈ 750K chars
        let total_chars: usize = self.state.history.iter()
            .map(|(_, c)| c.len())
            .sum();

        if total_chars > MAX_HISTORY_CHARS {
            // 保留最近 50% 的對話
            let keep = self.state.history.len() / 2;
            if keep > 0 {
                self.state.history = self.state.history
                    .split_off(self.state.history.len() - keep);
                tracing::warn!(
                    "[multi_agent] {} context pruned: kept last {} turns ({} chars)",
                    self.role, keep, total_chars
                );
            }
        }

        // 呼叫 Gemini API（用 tokio::task::block_in_place 處理 async）
        let http = crate::llm::shared_http();
        let llm = crate::llm::shared_llm();

        let output = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                crate::llm::call_prompt_messages(&http, &llm, &messages).await
            })
        })?;

        // 保存對話歷史
        self.state.history.push(("user".into(), body));
        self.state.history.push(("assistant".into(), output.clone()));

        // 生成 session_id（第一次）
        if is_new {
            self.state.session_id = Some(format!("gemini_{}", chrono::Local::now().timestamp()));
            self.state.role = self.role.clone();
            self.state.started_at = chrono::Local::now().to_rfc3339();
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
