//! 多 Agent 協作系統
//!
//! 三個持久化的 Claude Code session 分工合作：
//!
//! ```text
//! AgentTeam
//!   ├── PM       — 拆解任務、分配、review、記錄學習
//!   ├── Engineer — 執行開發工作
//!   └── Tester   — 執行測試、回報結果
//! ```
//!
//! 每個 session 使用 `claude -p ... --continue` 保持對話連續性。
//! session_id 存在磁碟，使用者可隨時 `claude --resume <id>` 查看對話。

mod roles;
mod session;

pub use session::PersistentSession;
use serde::Serialize;

// ── AgentTeam ─────────────────────────────────────────────────────────────────

pub struct AgentTeam {
    pub pm:       PersistentSession,
    pub engineer: PersistentSession,
    pub tester:   PersistentSession,
}

impl AgentTeam {
    /// 從磁碟還原（或建立全新的）team 狀態。
    /// `cwd` 是三個 session 共用的工作目錄（通常是 Sirin repo）。
    pub fn load(cwd: &str) -> Self {
        Self {
            pm:       PersistentSession::load("pm",       cwd, roles::PM),
            engineer: PersistentSession::load("engineer", cwd, roles::ENGINEER),
            tester:   PersistentSession::load("tester",   cwd, roles::TESTER),
        }
    }

    /// PM 分配任務 → 工程師執行 → PM review。
    /// 回傳 PM 最終的 review 結果。
    pub fn assign_task(&mut self, task: &str) -> Result<String, String> {
        // 1. PM 分析任務、出指令
        let plan = self.pm.send(
            &format!("新任務：{task}\n\n請拆解成具體步驟，給出明確指令讓工程師執行。")
        )?;

        // 2. 工程師執行
        let result = self.engineer.send(
            &format!("PM 指令：\n{plan}\n\n請開始執行，完成後回報結果。")
        )?;

        // 3. PM review，順便記錄學習
        let review = self.pm.send(
            &format!("工程師回報：\n{result}\n\n請 review。有問題指出具體修改方向；沒問題就核准。")
        )?;

        Ok(review)
    }

    /// 測試循環：Tester 執行 → 失敗則 Engineer 修 → PM 記錄。
    /// 回傳最終測試結果摘要。
    pub fn test_cycle(&mut self) -> Result<String, String> {
        // 1. Tester 跑測試
        let test_result = self.tester.send("請執行完整測試套件，回報結果。")?;

        // 2. 有失敗 → 工程師修
        let has_failure = test_result.contains("FAILED")
            || test_result.contains("failed")
            || test_result.contains("❌");

        if has_failure {
            let fix = self.engineer.send(
                &format!("Tester 回報測試失敗：\n{test_result}\n\n請修復失敗的測試。")
            )?;

            // 3. PM 記錄這次的錯誤與修復
            self.pm.send(
                &format!("測試失敗紀錄：\n失敗：{test_result}\n修復：{fix}\n\n請記錄這次的錯誤與修復方式。")
            )?;

            // 4. Tester 重新驗證
            let retest = self.tester.send("請重新執行測試，確認修復是否成功。")?;
            return Ok(retest);
        }

        Ok(test_result)
    }

    /// 取得三個 session 的摘要資訊（給 MCP / UI 顯示）。
    pub fn status(&self) -> TeamStatus {
        TeamStatus {
            pm:       session_info(&self.pm),
            engineer: session_info(&self.engineer),
            tester:   session_info(&self.tester),
        }
    }

    /// 重置指定角色的 session（開新對話）。
    pub fn reset_role(&mut self, role: &str) {
        match role {
            "pm"       => self.pm.reset(),
            "engineer" => self.engineer.reset(),
            "tester"   => self.tester.reset(),
            _ => {}
        }
    }
}

// ── 摘要資料結構（給 MCP / UI）────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TeamStatus {
    pub pm:       SessionInfo,
    pub engineer: SessionInfo,
    pub tester:   SessionInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub role:       String,
    pub session_id: Option<String>,
    pub turns:      u32,
    pub resume_cmd: String,
}

fn session_info(s: &PersistentSession) -> SessionInfo {
    let sid = s.session_id().map(|s| s.to_string());
    let resume = match &sid {
        Some(id) => format!("claude --resume {id}"),
        None     => "(尚未開始)".into(),
    };
    SessionInfo {
        role:       s.role.clone(),
        session_id: sid,
        turns:      s.turns(),
        resume_cmd: resume,
    }
}

// ── 全局單例（lazy init）──────────────────────────────────────────────────────

use std::sync::{Mutex, OnceLock};
static TEAM: OnceLock<Mutex<Option<AgentTeam>>> = OnceLock::new();

fn global() -> &'static Mutex<Option<AgentTeam>> {
    TEAM.get_or_init(|| Mutex::new(None))
}

/// 取得（或初始化）全局 AgentTeam。
pub fn get_or_init(cwd: &str) -> std::sync::MutexGuard<'static, Option<AgentTeam>> {
    let mut g = global().lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(AgentTeam::load(cwd));
    }
    g
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_session;

    #[test]
    fn session_info_no_id() {
        let s = PersistentSession::load("test_role", ".", "test prompt");
        let info = session_info(&s);
        assert_eq!(info.turns, 0);
        assert!(info.session_id.is_none());
        assert_eq!(info.resume_cmd, "(尚未開始)");
    }

    #[test]
    #[ignore] // needs Claude CLI + Max plan
    fn pm_send_creates_session_id() {
        let cwd = claude_session::repo_path("sirin").expect("sirin path");
        let mut pm = PersistentSession::load("pm_test", &cwd, roles::PM);
        pm.reset(); // fresh start

        let reply = pm.send("請用一句話介紹你自己。").expect("pm send");
        println!("PM reply: {reply}");
        println!("session_id: {:?}", pm.session_id());

        assert!(!reply.is_empty());
        assert!(pm.session_id().is_some(), "session_id should be captured");
        assert_eq!(pm.turns(), 1);

        // Second turn should continue the same session
        let reply2 = pm.send("你剛才說了什麼？").expect("pm send 2");
        println!("PM reply 2: {reply2}");
        assert_eq!(pm.turns(), 2);

        pm.reset(); // cleanup
    }

    #[test]
    #[ignore] // needs Claude CLI + Max plan
    fn agent_team_assign_task() {
        let cwd = claude_session::repo_path("sirin").expect("sirin path");
        let mut team = AgentTeam::load(&cwd);
        // Reset for clean test
        team.reset_role("pm");
        team.reset_role("engineer");

        let review = team.assign_task(
            "在 src/multi_agent/mod.rs 的 session_info() 函數加一行註解說明它的用途"
        ).expect("assign_task");
        println!("PM review: {review}");
        assert!(!review.is_empty());
    }
}
