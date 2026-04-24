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
pub mod github_adapter;
pub mod knowledge;
pub mod queue;
pub mod usage;
pub mod worker;
pub mod worktree;

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
    /// 從磁碟還原（或建立全新的）team 狀態（worker 0，向後相容）。
    /// `cwd` 是三個 session 共用的工作目錄（通常是 Sirin repo）。
    pub fn load(cwd: &str) -> Self {
        Self::load_for_worker(cwd, 0)
    }

    /// 從磁碟還原指定 worker 的 team 狀態。
    ///
    /// `worker_id == 0` 走原有 session 檔（向後相容），`worker_id >= 1`
    /// 走 `w{worker_id}_{role}.json` 命名空間 — 多 worker 平行時各自獨立。
    pub fn load_for_worker(cwd: &str, worker_id: usize) -> Self {
        Self::load_for_worker_project(cwd, worker_id, "")
    }

    /// 從磁碟還原指定 (worker, project) 的 team 狀態。
    ///
    /// `project_key` 為空字串或 "sirin" 時與 `load_for_worker` 結果完全相同
    /// （走 legacy session 檔名）。其他值如 "agora_market" 會在檔名加
    /// `p{key}_` 命名空間，讓不同專案的 PM/Engineer/Tester 對話歷史互不干擾
    /// — 例如 PM 對 Sirin 與 PM 對 AgoraMarket 是獨立 session_id。
    pub fn load_for_worker_project(cwd: &str, worker_id: usize, project_key: &str) -> Self {
        Self {
            pm: PersistentSession::load_for_worker_project(
                "pm",       worker_id, project_key, cwd, roles::PM,
            ),
            engineer: PersistentSession::load_for_worker_project(
                "engineer", worker_id, project_key, cwd, roles::ENGINEER,
            ),
            tester: PersistentSession::load_for_worker_project(
                "tester",   worker_id, project_key, cwd, roles::TESTER,
            ),
        }
    }

    /// 為下一個任務動態設定 extra tools（套用於 PM / Engineer / Tester 全部）。
    ///
    /// 用於 `ProjectContext.extra_tools` — 例如要讓 Engineer 透過 `gh` CLI
    /// 操作 GitHub issue 時，傳 `&["Bash".to_string()]`（PM 也會拿到，雖然
    /// 它的靜態 whitelist 沒有 Bash）。
    ///
    /// 不會持久化；reload 後重置為空。每個任務開始前由 worker 呼叫一次，
    /// 結束後再以 `&[]` 清掉。
    pub fn set_extra_tools(&mut self, extra: &[String]) {
        self.pm.extra_tools       = extra.to_vec();
        self.engineer.extra_tools = extra.to_vec();
        self.tester.extra_tools   = extra.to_vec();
    }

    /// 為下一個任務啟用/停用 dry-run 驗證模式（套用於 PM / Engineer / Tester
    /// 三個 session）。詳細語意見 `queue::ProjectContext::dry_run` 文件。
    ///
    /// 不會持久化；reload 後重置為 `false`。每個任務開始前由 worker 呼叫一次，
    /// 結束後再以 `false` 清掉。
    pub fn set_dry_run(&mut self, dry_run: bool) {
        self.pm.dry_run       = dry_run;
        self.engineer.dry_run = dry_run;
        self.tester.dry_run   = dry_run;
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
        const MAX_ITER:      usize = 5;      // was 3; UI/multi-file tasks need more room
        const MAX_MSG_CHARS: usize = 120_000; // Gemini 1M window — 可以處理更長的 context

        // 安全截斷 helper（找 max_bytes 內最後一個 char boundary）
        fn trunc(s: &str, max: usize) -> &str {
            let end = s.len().min(max);
            let b = (0..=end).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
            &s[..b]
        }

        // 1. PM 分析任務、拆解指令（注入歷史知識）
        let lessons = knowledge::relevant_lessons(task, 5);
        let knowledge_prefix = knowledge::format_knowledge_prefix(&lessons);
        let plan = self.pm.send(
            &format!("{knowledge_prefix}新任務：{task}\n\n請拆解成具體步驟，給出明確指令讓工程師執行。")
        )?;
        let plan_short = trunc(&plan, MAX_MSG_CHARS);

        let mut last_review = String::new();

        for iter in 0..MAX_ITER {
            // T1-4: Don't reset engineer between iterations of the SAME task —
            // engineer retains context (what it tried, what failed) across retries.
            // Cross-task context bloat is handled by worker.rs: turns > 20 → reset.

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
            // T1-6: structured token takes priority; old keyword is fallback for
            // sessions that pre-date this prompt update (no <<<VERDICT>>> in prompt).
            let approved = review.contains("<<<VERDICT: APPROVED>>>")
                || (review.contains("核准") && !review.contains("<<<VERDICT: NEEDS_FIX"));

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

    /// T2-2: 在 Engineer 完成任務後，自動驗證指定 YAML test 是否通過。
    ///
    /// # 流程
    /// 1. 從 `sirin_cwd/config/tests/` 遞迴搜尋 `{test_id}.yaml`
    /// 2. 以 `spawn_adhoc_run` 直接執行（不需 sync 到 LocalAppData）
    /// 3. 輪詢直到 terminal state（5 min 超時）
    /// 4. 通過 → 回傳成功摘要；失敗 → Engineer 修 YAML → 再試一次
    ///
    /// `sirin_cwd` 是 Sirin repo 根目錄（例如 `C:/repos/Sirin`）。
    /// `test_id` 是 YAML 的 `id` 欄位（不含 `.yaml`）。
    pub fn yaml_test_cycle(&mut self, sirin_cwd: &str, test_id: &str) -> Result<String, String> {
        use std::time::{Duration, Instant};
        use crate::test_runner::{AdhocRunRequest, TestStatus};
        use crate::test_runner::runs::RunPhase;

        const MAX_ATTEMPTS:       usize    = 2;
        const POLL_INTERVAL:      Duration = Duration::from_secs(5);
        const TEST_TIMEOUT_SECS:  u64      = 300;

        // 遞迴搜尋 {sirin_cwd}/config/tests/**/{test_id}.yaml
        fn find_yaml(dir: &std::path::Path, test_id: &str) -> Option<std::path::PathBuf> {
            let direct = dir.join(format!("{test_id}.yaml"));
            if direct.exists() { return Some(direct); }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        if let Some(f) = find_yaml(&p, test_id) { return Some(f); }
                    }
                }
            }
            None
        }

        let yaml_dir = std::path::PathBuf::from(sirin_cwd).join("config").join("tests");

        for attempt in 0..MAX_ATTEMPTS {
            // 1. 載入 YAML（每次 attempt 都重載，讓 Engineer 修完後能看到新版本）
            let yaml_path = match find_yaml(&yaml_dir, test_id) {
                Some(p) => p,
                None => return Err(format!(
                    "yaml_test_cycle: `{test_id}.yaml` 不在 {yaml_dir:?} 或子目錄"
                )),
            };
            let goal = crate::test_runner::parser::load_file(&yaml_path)
                .map_err(|e| format!("yaml_test_cycle: load YAML 失敗: {e}"))?;

            // 2. 執行 adhoc run（不需先 sync 到 LocalAppData）
            let req = AdhocRunRequest {
                url:              goal.url.clone(),
                goal:             goal.goal.clone(),
                success_criteria: goal.success_criteria.clone(),
                locale:           Some(goal.locale.clone()),
                max_iterations:   Some(goal.max_iterations),
                timeout_secs:     Some(goal.timeout_secs),
                browser_headless: goal.browser_headless,
                llm_backend:      goal.llm_backend.clone(),
                ..Default::default()
            };
            let run_id = crate::test_runner::spawn_adhoc_run(req)
                .map_err(|e| format!("yaml_test_cycle: spawn 失敗: {e}"))?;

            tracing::info!(target: "sirin",
                "[yaml-test] attempt {}/{}: test_id={test_id} run_id={run_id}",
                attempt + 1, MAX_ATTEMPTS);

            // 3. 輪詢直到 terminal
            let deadline = Instant::now() + Duration::from_secs(TEST_TIMEOUT_SECS);
            let result = loop {
                std::thread::sleep(POLL_INTERVAL);
                let state = match crate::test_runner::runs::get(&run_id) {
                    Some(s) => s,
                    None => return Err(format!("yaml_test_cycle: run {run_id} 消失於 registry")),
                };
                match state.phase {
                    RunPhase::Complete(r) => break r,
                    RunPhase::Error(e)    => return Err(format!("yaml_test_cycle: run errored: {e}")),
                    _ if Instant::now() >= deadline => {
                        return Err(format!(
                            "yaml_test_cycle: `{test_id}` 超時（{TEST_TIMEOUT_SECS}s）"
                        ));
                    }
                    _ => continue,
                }
            };

            // 4. 判斷結果
            let passed = matches!(result.status, TestStatus::Passed);
            if passed {
                let summary = format!(
                    "[Tester ✅] YAML test `{test_id}` 通過（{} iterations, {:.1}s）",
                    result.iterations,
                    result.duration_ms as f64 / 1000.0
                );
                let _ = self.tester.send(&format!(
                    "YAML 驗證結果：{summary}。測試通過，任務完成。"
                ));
                let _ = self.pm.send(&format!(
                    "YAML 自動驗證結果：{summary}\n\
                     [📝 學到: YAML test `{test_id}` 在 {} iterations 內通過，目前 YAML 設計正確]",
                    result.iterations
                ));
                return Ok(summary);
            }

            // 5. 失敗 → Engineer 修 YAML（若還有下一輪）
            if attempt + 1 < MAX_ATTEMPTS {
                let failure_info = result.error_message.as_deref()
                    .or(result.final_analysis.as_deref())
                    .unwrap_or("（無具體錯誤訊息）");

                tracing::warn!(target: "sirin",
                    "[yaml-test] attempt {}/{} FAILED: {failure_info}",
                    attempt + 1, MAX_ATTEMPTS);

                let engineer_msg = format!(
                    "YAML test `{test_id}` 第 {}/{} 次驗證失敗（{} iterations）。\n\
                     失敗訊息：{failure_info}\n\n\
                     請修復 `config/tests/**/{test_id}.yaml`，注意以下原則：\n\
                     - 步驟必須線性，不寫 if/else 分支\n\
                     - `done=true` 放在固定最後一步（無條件）\n\
                     - goal text 不寫 JSON，scroll 用中文描述（例：向下捲動 500px（scroll y=500））\n\
                     - `max_iterations` = 步驟數 × 2\n\
                     - Flutter AppBar 返回：用 `eval target='window.history.back()'`\n\
                     修完後直接回報改了什麼（無需等候，系統會自動重新驗證）。",
                    attempt + 1, MAX_ATTEMPTS, result.iterations
                );
                let _ = self.engineer.send(&engineer_msg);
                // 短暫等候 Engineer 有機會修改（工程師 session 非同步）
                std::thread::sleep(Duration::from_secs(30));
            }
        }

        Err(format!(
            "yaml_test_cycle: `{test_id}` 在 {MAX_ATTEMPTS} 次嘗試後仍未通過"
        ))
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
