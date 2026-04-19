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
        let path = state_path_for(role, worker_id);
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
            state,
        }
    }

    /// 送一條訊息給這個 session，回傳助理的回覆。
    /// 第一次呼叫時會在訊息前加上 system_prompt。
    pub fn send(&mut self, message: &str) -> Result<String, String> {
        let is_new   = self.state.session_id.is_none();
        let prompt   = if is_new {
            format!("{}\n\n---\n\n{}", self.system_prompt.trim(), message)
        } else {
            message.to_string()
        };

        // Select per-role tool whitelist; unknown roles keep god mode to avoid
        // accidentally locking out future roles.
        let whitelist: Option<&[&str]> = match self.role.as_str() {
            "pm"       => Some(crate::multi_agent::roles::ALLOWED_TOOLS_PM),
            "tester"   => Some(crate::multi_agent::roles::ALLOWED_TOOLS_TESTER),
            "engineer" => Some(crate::multi_agent::roles::ALLOWED_TOOLS_ENGINEER),
            _          => None,
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
        let _ = std::fs::remove_file(state_path_for(&self.role, self.worker_id));
    }

    fn save(&self) {
        let path = state_path_for(&self.role, self.worker_id);
        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Ok(json) = serde_json::to_string_pretty(&self.state) {
            let _ = std::fs::write(&path, json);
        }
    }
}

/// State file path for a (role, worker_id) pair.
///
/// Worker 0 uses the legacy path `data/multi_agent/{role}.json` so existing
/// PM/Engineer/Tester history survives the upgrade.  Workers 1+ use namespaced
/// paths `data/multi_agent/w{worker_id}_{role}.json`.
fn state_path_for(role: &str, worker_id: usize) -> PathBuf {
    let filename = if worker_id == 0 {
        format!("{role}.json")
    } else {
        format!("w{worker_id}_{role}.json")
    };
    platform::app_data_dir()
        .join("data")
        .join("multi_agent")
        .join(filename)
}
