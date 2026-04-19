//! 背景工作執行緒 — 持續消費任務佇列。
//!
//! 啟動後進入無窮循環：
//!   1. 從 `queue` **原子地**取出下一個 Queued 任務（已標記 Running）
//!   2. 呼叫 `AgentTeam::assign_task()`（PM → Engineer → PM review）
//!   3. 任務完成後呼叫 `test_cycle()` 驗證編譯
//!   4. 更新任務狀態（Done / Failed）
//!   5. 沒有任務時每 10 秒輪詢一次
//!
//! 用法：
//! ```rust
//! multi_agent::worker::spawn("C:/repos/Sirin");          // 1 worker（向後相容）
//! multi_agent::worker::spawn_n("C:/repos/Sirin", 3);     // 3 平行 worker
//! ```
//!
//! ## 多 Worker 注意事項（T1-1）
//!
//! - 每個 worker 有自己的 `AgentTeam`（PM/Engineer/Tester），session 檔分別存
//!   在 `data/multi_agent/w{N}_{role}.json`（worker 0 走原 path，向後相容）。
//! - `queue::take_next_queued()` 是原子操作，不會兩個 worker 搶到同個 task。
//! - 三個 worker **共用同一 cwd**（Sirin repo），改不同檔通常 OK；改同檔會
//!   git-stage 衝突。徹底解決靠 T2-4 worktree 隔離。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use super::{AgentTeam, queue, queue::TaskStatus};

static STARTED: AtomicBool = AtomicBool::new(false);

/// Worker 執行緒是否已啟動（給 UI / 其他模組查詢）。
pub fn is_running() -> bool {
    STARTED.load(Ordering::Relaxed)
}

/// 啟動 1 個持續工作執行緒（向後相容 wrapper）。
pub fn spawn(cwd: &str) {
    spawn_n(cwd, 1);
}

/// 啟動 `n` 個平行工作執行緒（只呼叫一次，重複呼叫為 no-op）。
/// 已有 Running 狀態的任務會先被重置為 Queued（防止上次崩潰留下殘留）。
///
/// `n` 範圍實務上 1-8；超過會撞 Anthropic API rate limit。建議從 2 起跳。
pub fn spawn_n(cwd: &str, n: usize) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return; // 已啟動，idempotent
    }
    // 把上次未完成的 Running 任務重設為 Queued
    reset_stale_running();

    let n = n.max(1);  // safety: 至少 1 個 worker
    for w in 0..n {
        let cwd = cwd.to_string();
        std::thread::Builder::new()
            .name(format!("multi-agent-worker-{w}"))
            .spawn(move || run_loop(cwd, w))
            .expect("spawn multi-agent worker");
    }

    tracing::info!(target: "sirin",
        "[team-worker] Started {n} worker(s) — polling queue every 10s");
}

// ── Main loop ─────────────────────────────────────────────────────────────────

fn run_loop(cwd: String, worker_id: usize) {
    let mut team = AgentTeam::load_for_worker(&cwd, worker_id);

    loop {
        // 原子操作：取任務同時標記 Running，多 worker 安全
        match queue::take_next_queued() {
            Some(task) => {
                // 安全截斷：找 80 bytes 內最後的 char boundary
                let preview_end = {
                    let max = task.description.len().min(80);
                    (0..=max).rev().find(|&i| task.description.is_char_boundary(i)).unwrap_or(0)
                };
                tracing::info!(target: "sirin",
                    "[team-worker:w{worker_id}] Starting task {} — {}",
                    task.id, &task.description[..preview_end]);

                match team.assign_task(&task.description) {
                    Ok(review) => {
                        tracing::info!(target: "sirin",
                            "[team-worker:w{worker_id}] Task {} done ✓", task.id);
                        queue::update_status(&task.id, TaskStatus::Done, Some(review));

                        // 驗證編譯（cargo check）
                        if let Err(e) = team.test_cycle() {
                            tracing::warn!(target: "sirin",
                                "[team-worker:w{worker_id}] test_cycle after task {}: {e}", task.id);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "sirin",
                            "[team-worker:w{worker_id}] Task {} failed: {e}", task.id);
                        queue::update_status(&task.id, TaskStatus::Failed, Some(e));
                    }
                }

                // Engineer context window 保護：超過 40 輪就開新 session
                // (raised from 20 → 40 because T1-4 keeps context within a task,
                //  so a 5-iter task can use up to 5 turns before inter-task reset)
                if team.engineer.turns() > 40 {
                    tracing::info!(target: "sirin",
                        "[team-worker:w{worker_id}] Engineer turns > 40 — resetting for fresh context");
                    team.engineer.reset();
                }
            }
            None => {
                // 沒任務，休眠等待
                std::thread::sleep(Duration::from_secs(10));
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// 把上次崩潰留下的 Running 任務重設為 Queued。
fn reset_stale_running() {
    let running = queue::list_by_status(&TaskStatus::Running);
    for t in running {
        tracing::warn!(target: "sirin",
            "[team-worker] Resetting stale Running task {} to Queued", t.id);
        queue::update_status(&t.id, TaskStatus::Queued, None);
    }
}
