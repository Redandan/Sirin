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
pub mod queue;
pub mod worker;

pub use session::PersistentSession;
pub use queue::TaskStatus;
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

    /// PM 分配任務 → 工程師執行 → PM review → (不通過則迭代修改)。
    ///
    /// 最多執行 `MAX_ITER` 輪 Engineer → PM review 循環。
    /// PM 回覆中包含「[PM ✓」或「核准」即視為通過，提前結束。
    /// 超過輪次上限回傳 Err。
    ///
    /// Context overflow prevention:
    /// - Engineer session 每輪重置（避免 --continue 累積歷史爆 context window）
    /// - 傳入各 session 的訊息截斷至 MAX_MSG_CHARS
    pub fn assign_task(&mut self, task: &str) -> Result<String, String> {
        const MAX_ITER:      usize = 3;
        const MAX_MSG_CHARS: usize = 8_000; // 每條訊息上限，避免 "Prompt is too long"

        // 安全截斷 helper（找 max_bytes 內最後一個 char boundary）
        fn trunc(s: &str, max: usize) -> &str {
            let end = s.len().min(max);
            let b = (0..=end).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
            &s[..b]
        }

        // 1. PM 分析任務、拆解指令
        let plan = self.pm.send(
            &format!("新任務：{task}\n\n請拆解成具體步驟，給出明確指令讓工程師執行。")
        )?;
        let plan_short = trunc(&plan, MAX_MSG_CHARS);

        let mut last_review = String::new();

        for iter in 0..MAX_ITER {
            // 每輪重置 engineer session，防止 --continue 歷史無限累積
            if iter > 0 {
                self.engineer.reset();
            }

            // 2. 工程師執行（每輪從 task + plan + PM feedback 重建 context）
            let engineer_msg = if iter == 0 {
                format!("PM 指令：\n{plan_short}\n\n請開始執行，完成後回報結果。")
            } else {
                let review_short = trunc(&last_review, MAX_MSG_CHARS / 2);
                format!(
                    "任務：{task}\n\nPM 指令（摘要）：\n{plan_short}\n\nPM Review（第 {iter} 輪未通過），修改要求：\n{review_short}\n\n請修正後重新執行並回報。"
                )
            };
            let result = self.engineer.send(&engineer_msg)?;
            let result_short = trunc(&result, MAX_MSG_CHARS);

            // 3. PM review
            let review = self.pm.send(
                &format!("工程師回報（第 {} 輪）：\n{result_short}\n\n請 review。有問題指出具體修改方向；沒問題就核准。",
                    iter + 1)
            )?;

            // 4. 判斷是否核准
            let approved = review.contains("[PM ✓")
                || review.contains("核准")
                || review.contains("Approved")
                || review.contains("LGTM");

            if approved {
                return Ok(review);
            }

            // 5. 未核准 — 記錄 PM 意見供下輪使用
            last_review = review;
        }

        // 超過輪次但有最後一次 review，仍回傳（讓呼叫端決定怎麼處理）
        if last_review.is_empty() {
            Err(format!("assign_task: {MAX_ITER} 輪後 PM 仍未核准"))
        } else {
            Err(format!("assign_task: {MAX_ITER} 輪後 PM 仍未核准\n最後 review：\n{last_review}"))
        }
    }

    /// 測試循環：Tester 執行 cargo check → 失敗則 Engineer 修 → PM 記錄。
    /// 回傳最終驗證結果摘要。
    ///
    /// ⚠️  使用 cargo check 而非 cargo test，避免與呼叫者的 cargo 進程產生
    ///     file lock 衝突（cargo 使用排他鎖，重入會造成死鎖）。
    pub fn test_cycle(&mut self) -> Result<String, String> {
        // 1. Tester 驗證編譯
        let test_result = self.tester.send(
            "請執行 cargo check 驗證編譯，回報結果（0 errors = 通過）。"
        )?;

        // 2. 有失敗 → 工程師修
        // 偵測模式涵蓋：cargo check 錯誤、cargo test 失敗、Tester 回報格式
        let has_failure = test_result.contains("FAILED")
            || test_result.contains("failed")
            || test_result.contains("❌")
            || test_result.contains("error[")     // Rust 編譯錯誤 (e.g. error[E0308])
            || test_result.contains("error: ")    // Rust linker / proc-macro 錯誤
            || test_result.contains("编译失败");   // Tester 中文回報

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

    /// 執行佇列裡的所有 Queued 任務（用 worker 循環）
    /// 適合：先用 enqueue_three_tasks 推任務，再跑這個讓小隊工作
    #[test]
    #[ignore] // long-running; modifies real source files
    fn run_queued_tasks() {
        use std::time::{Duration, Instant};
        let cwd = claude_session::repo_path("sirin").expect("sirin path");

        // 把上次崩潰或中斷留下的 Running 任務重設為 Queued
        for t in queue::list_by_status(&queue::TaskStatus::Running) {
            println!("⚠ 重設殘留 Running 任務 {} → Queued", t.id);
            queue::update_status(&t.id, queue::TaskStatus::Queued, None);
        }

        let pending = queue::list_by_status(&queue::TaskStatus::Queued);
        if pending.is_empty() {
            println!("佇列是空的，沒有任務可以執行");
            return;
        }
        println!("=== 小隊開始工作：{} 個 Queued 任務 ===", pending.len());

        let mut team = AgentTeam::load(&cwd);
        let timeout = Duration::from_secs(60 * 30); // 最多 30 分鐘
        let start = Instant::now();

        loop {
            let Some(task) = queue::next_queued() else {
                println!("\n✅ 佇列清空，所有任務處理完畢");
                break;
            };
            if start.elapsed() > timeout {
                println!("\n⏰ 超時（30 分鐘），停止");
                break;
            }

            println!("\n─── 任務 {} ───", &task.id);
            println!("描述：{:.100}", task.description);
            queue::update_status(&task.id, queue::TaskStatus::Running, None);

            match team.assign_task(&task.description) {
                Ok(review) => {
                    // 安全截斷：尋找 300 bytes 內最後一個有效 char boundary
                    let end = {
                        let max = review.len().min(300);
                        (0..=max).rev().find(|&i| review.is_char_boundary(i)).unwrap_or(0)
                    };
                    println!("[✓ 完成]\n{}", &review[..end]);
                    queue::update_status(&task.id, queue::TaskStatus::Done, Some(review));
                    let _ = team.test_cycle();
                }
                Err(e) => {
                    println!("[✗ 失敗] {e}");
                    queue::update_status(&task.id, queue::TaskStatus::Failed, Some(e));
                }
            }
            // Engineer context 保護
            if team.engineer.turns() > 20 { team.engineer.reset(); }
        }

        // 印最終佇列狀態
        let all = queue::list_all();
        println!("\n=== 佇列最終狀態 ===");
        for t in &all {
            println!("  [{}] {:.60}", t.status, t.description);
        }
    }

    /// GUI 優化任務 — 由 PM / Engineer / Tester 小隊執行
    /// 任務：改善 workspace.rs 聊天 tab 的 UX
    #[test]
    #[ignore] // needs Claude CLI + Max plan; modifies real source files
    fn gui_optimization_chat_tab() {
        let cwd = claude_session::repo_path("sirin").expect("sirin path");
        let mut team = AgentTeam::load(&cwd);
        team.reset_role("pm");
        team.reset_role("engineer");
        team.reset_role("tester");

        let task = r#"
GUI 優化需求：改善 src/ui_egui/workspace.rs 的聊天 tab（show_chat 函數）

== 背景 ==
目前聊天 tab 存在以下問題：
1. 輸入框是單行（TextEdit::singleline），長訊息不方便輸入
2. 訊息沒有時間戳，看不出何時發送
3. 沒有清空對話的方法

== 具體修改（只改 src/ui_egui/workspace.rs）==

**修改 1：多行輸入框**
- 將 TextEdit::singleline 改為 TextEdit::multiline，高度設為 72px
- 保持 Enter 鍵送出（檢測 Key::Enter 且 !modifiers.shift）
- Shift+Enter 換行（egui multiline 預設行為，不需額外處理）

**修改 2：訊息時間戳**
- WorkspaceState.chat_history 的型別從 Vec<(String, String)>
  改為 Vec<(String, String, String)>（role, text, timestamp）
- 發送/接收訊息時 timestamp = chrono::Local::now().format("%H:%M").to_string()
- 每條訊息 card 下方右對齊顯示時間戳：
  RichText::new(&ts).size(theme::FONT_CAPTION).color(theme::TEXT_DIM)

**修改 3：清空對話按鈕**
- 輸入框左側加「清空」按鈕（只在 chat_history 非空時顯示）
- 使用 theme::DANGER.linear_multiply(0.7) 顏色
- 點擊後 state.chat_history.clear()

== 限制 ==
- 只修改 src/ui_egui/workspace.rs，不動其他檔案
- 修改完後必須執行 cargo check 確認 0 errors
- 保持現有 theme::* 常數不直接寫顏色數值
        "#;

        println!("=== 啟動 GUI 優化任務 ===\n");

        // PM 分析 → Engineer 實作 → PM review
        println!("--- PM 分析任務中 ---");
        let review = team.assign_task(task).expect("assign_task");
        println!("\n=== PM 最終 Review ===\n{review}");

        // 執行完後讓 Tester 驗證（cargo check，不會有 lock 衝突）
        println!("\n--- Tester 驗證 ---");
        let test_result = team.test_cycle().expect("test_cycle");
        println!("\n=== Tester 報告 ===\n{test_result}");

        assert!(!review.is_empty(), "PM review should not be empty");
    }
}
