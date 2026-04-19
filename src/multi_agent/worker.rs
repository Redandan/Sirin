//! 背景工作執行緒 — 持續消費任務佇列。
//!
//! 啟動後進入無窮循環：
//!   1. 從 `queue` 取出下一個 Queued 任務
//!   2. 呼叫 `AgentTeam::assign_task()`（PM → Engineer → PM review）
//!   3. 任務完成後呼叫 `test_cycle()` 驗證編譯
//!   4. 更新任務狀態（Done / Failed）
//!   5. 沒有任務時每 10 秒輪詢一次
//!
//! 用法：
//! ```rust
//! multi_agent::worker::spawn("C:/repos/Sirin");
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use super::{AgentTeam, queue, queue::TaskStatus};

static STARTED: AtomicBool = AtomicBool::new(false);

/// Worker 執行緒是否已啟動（給 UI / 其他模組查詢）。
pub fn is_running() -> bool {
    STARTED.load(Ordering::Relaxed)
}

/// 啟動持續工作執行緒（只呼叫一次，重複呼叫為 no-op）。
/// 已有 Running 狀態的任務會先被重置為 Queued（防止上次崩潰留下殘留）。
pub fn spawn(cwd: &str) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return; // 已啟動，idempotent
    }
    // 把上次未完成的 Running 任務重設為 Queued
    reset_stale_running();

    let cwd = cwd.to_string();
    std::thread::Builder::new()
        .name("multi-agent-worker".into())
        .spawn(move || run_loop(cwd))
        .expect("spawn multi-agent worker");

    tracing::info!(target: "sirin", "[team-worker] Started — polling queue every 10s");
}

// ── Main loop ─────────────────────────────────────────────────────────────────

fn run_loop(cwd: String) {
    let mut team = AgentTeam::load(&cwd);

    loop {
        match queue::next_queued() {
            Some(task) => {
                // 安全截斷：找 80 bytes 內最後的 char boundary
                let preview_end = {
                    let max = task.description.len().min(80);
                    (0..=max).rev().find(|&i| task.description.is_char_boundary(i)).unwrap_or(0)
                };
                tracing::info!(target: "sirin",
                    "[team-worker] Starting task {} — {}", task.id, &task.description[..preview_end]);

                queue::update_status(&task.id, TaskStatus::Running, None);

                match team.assign_task(&task.description) {
                    Ok(review) => {
                        tracing::info!(target: "sirin",
                            "[team-worker] Task {} done ✓", task.id);
                        queue::update_status(&task.id, TaskStatus::Done, Some(review));

                        // 驗證編譯（cargo check）
                        if let Err(e) = team.test_cycle() {
                            tracing::warn!(target: "sirin",
                                "[team-worker] test_cycle after task {}: {e}", task.id);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "sirin",
                            "[team-worker] Task {} failed: {e}", task.id);
                        queue::update_status(&task.id, TaskStatus::Failed, Some(e));
                    }
                }

                // Engineer context window 保護：超過 20 輪就開新 session
                if team.engineer.turns() > 20 {
                    tracing::info!(target: "sirin",
                        "[team-worker] Engineer turns > 20 — resetting for fresh context");
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
