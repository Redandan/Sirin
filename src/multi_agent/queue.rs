//! 持久化任務佇列 — JSONL 格式存在磁碟。
//!
//! 每一行是一個 `TeamTask` JSON 物件。
//! 狀態流：Queued → Running → Done | Failed
//!
//! 使用全局 Mutex 確保多執行緒安全。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use crate::platform;

// ── 資料結構 ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Queued,
    Running,
    Done,
    Failed,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Queued  => write!(f, "queued"),
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Done    => write!(f, "done"),
            TaskStatus::Failed  => write!(f, "failed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTask {
    pub id:          String,
    pub description: String,
    pub created_at:  String,
    pub status:      TaskStatus,
    pub result:      Option<String>,
    pub finished_at: Option<String>,
    #[serde(default)]
    pub retry_count: u8,
}

// ── 全局鎖 ────────────────────────────────────────────────────────────────────

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock() -> &'static Mutex<()> {
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── 路徑 ──────────────────────────────────────────────────────────────────────

fn queue_path() -> PathBuf {
    platform::app_data_dir()
        .join("data")
        .join("multi_agent")
        .join("task_queue.jsonl")
}

// ── Public API ────────────────────────────────────────────────────────────────

/// 加入新任務。回傳任務 ID（毫秒時間戳）。
pub fn enqueue(description: &str) -> String {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    let id = chrono::Local::now().timestamp_millis().to_string();
    let task = TeamTask {
        id:          id.clone(),
        description: description.to_string(),
        created_at:  chrono::Local::now().to_rfc3339(),
        status:      TaskStatus::Queued,
        result:      None,
        finished_at: None,
        retry_count: 0,
    };
    append_unlocked(&task);
    id
}

/// 加入新任務並帶入指定 retry_count（內部使用，供 auto-retry 機制呼叫）。
pub fn enqueue_with_retry(description: &str, retry_count: u8) -> String {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    let id = chrono::Local::now().timestamp_millis().to_string();
    let task = TeamTask {
        id:          id.clone(),
        description: description.to_string(),
        created_at:  chrono::Local::now().to_rfc3339(),
        status:      TaskStatus::Queued,
        result:      None,
        finished_at: None,
        retry_count,
    };
    append_unlocked(&task);
    id
}

/// 取得下一個 Queued 狀態的任務（不 pop，需呼叫 `update_status` 標記 Running）。
///
/// ⚠️  非原子！多 worker 環境會兩個 worker 搶到同個任務。
/// 多 worker 請改用 [`take_next_queued`]。
pub fn next_queued() -> Option<TeamTask> {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    read_all_unlocked().into_iter().find(|t| t.status == TaskStatus::Queued)
}

/// 原子操作：取出最早的 Queued 任務，**同時**把它標記為 Running，回寫磁碟。
///
/// 多 worker 平行執行時必用此函數，避免兩個 worker 搶到同個任務。
/// 整個 read → mutate → rewrite 在同一個 LOCK 內完成。
///
/// 回傳的 `TeamTask` 已是 Running 狀態（`status` 欄位已更新）。
pub fn take_next_queued() -> Option<TeamTask> {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut tasks = read_all_unlocked();
    let mut found_idx = None;
    for (i, t) in tasks.iter().enumerate() {
        if t.status == TaskStatus::Queued {
            found_idx = Some(i);
            break;
        }
    }
    if let Some(i) = found_idx {
        tasks[i].status = TaskStatus::Running;
        let taken = tasks[i].clone();
        rewrite_unlocked(&tasks);
        Some(taken)
    } else {
        None
    }
}

/// 更新任務狀態（Done / Failed 時自動記錄完成時間）。
pub fn update_status(id: &str, status: TaskStatus, result: Option<String>) {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut tasks = read_all_unlocked();
    for t in &mut tasks {
        if t.id == id {
            t.status      = status.clone();
            t.result      = result.clone();
            if matches!(status, TaskStatus::Done | TaskStatus::Failed) {
                t.finished_at = Some(chrono::Local::now().to_rfc3339());
            }
        }
    }
    rewrite_unlocked(&tasks);
}

/// 列出所有任務（最新的在前）。
pub fn list_all() -> Vec<TeamTask> {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut v = read_all_unlocked();
    v.reverse();
    v
}

/// 列出指定狀態的任務。
pub fn list_by_status(status: &TaskStatus) -> Vec<TeamTask> {
    list_all().into_iter().filter(|t| &t.status == status).collect()
}

/// 清除所有 Done / Failed 任務（保留 Queued / Running）。
pub fn clear_completed() {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    let tasks: Vec<TeamTask> = read_all_unlocked()
        .into_iter()
        .filter(|t| matches!(t.status, TaskStatus::Queued | TaskStatus::Running))
        .collect();
    rewrite_unlocked(&tasks);
}

// ── 內部 helpers（呼叫前必須持有 LOCK）───────────────────────────────────────

fn read_all_unlocked() -> Vec<TeamTask> {
    let path = queue_path();
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn append_unlocked(task: &TeamTask) {
    let path = queue_path();
    if let Some(p) = path.parent() { let _ = std::fs::create_dir_all(p); }
    let line = serde_json::to_string(task).unwrap_or_default();
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::write(&path, format!("{content}{line}\n"));
}

fn rewrite_unlocked(tasks: &[TeamTask]) {
    let path = queue_path();
    let content = tasks.iter()
        .filter_map(|t| serde_json::to_string(t).ok())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(&path, format!("{content}\n"));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 每個測試用不同路徑，避免衝突。
    /// （platform::app_data_dir() 在測試中返回 ./config，但 queue_path 走 app_data_dir/data/...）

    #[test]
    fn enqueue_and_list() {
        // 清空再測
        {
            let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
            let _ = std::fs::remove_file(queue_path());
        }

        let id = enqueue("測試任務 A");
        assert!(!id.is_empty());

        let tasks = list_all();
        assert!(!tasks.is_empty());
        assert_eq!(tasks[0].description, "測試任務 A");
        assert_eq!(tasks[0].status, TaskStatus::Queued);
    }

    #[test]
    fn next_queued_returns_oldest() {
        {
            let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
            let _ = std::fs::remove_file(queue_path());
        }

        // 稍微錯開 ID（timestamp_millis 可能相同）
        static CTR: AtomicU64 = AtomicU64::new(0);
        let base = chrono::Local::now().timestamp_millis() as u64;

        {
            let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
            let t1 = TeamTask {
                id: (base + CTR.fetch_add(1, Ordering::SeqCst)).to_string(),
                description: "first".into(), created_at: "".into(),
                status: TaskStatus::Queued, result: None, finished_at: None,
                retry_count: 0,
            };
            let t2 = TeamTask {
                id: (base + CTR.fetch_add(1, Ordering::SeqCst)).to_string(),
                description: "second".into(), created_at: "".into(),
                status: TaskStatus::Queued, result: None, finished_at: None,
                retry_count: 0,
            };
            append_unlocked(&t1);
            append_unlocked(&t2);
        }

        let next = next_queued().expect("should have queued task");
        assert_eq!(next.description, "first");
    }

    #[test]
    fn update_status_marks_done() {
        {
            let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
            let _ = std::fs::remove_file(queue_path());
        }

        let id = enqueue("status test");
        update_status(&id, TaskStatus::Running, None);
        let tasks = list_all();
        assert_eq!(tasks[0].status, TaskStatus::Running);

        update_status(&id, TaskStatus::Done, Some("ok".into()));
        let tasks = list_all();
        assert_eq!(tasks[0].status, TaskStatus::Done);
        assert_eq!(tasks[0].result.as_deref(), Some("ok"));
        assert!(tasks[0].finished_at.is_some());
    }

    /// 把三個 GUI 優化任務推進佇列。
    /// 執行後確認佇列有 3 筆 Queued 任務，再用 agent_start_worker 啟動 Worker。
    #[test]
    #[ignore] // 直接寫入 production queue — 只在需要時手動執行
    fn enqueue_three_tasks() {
        let t1 = enqueue(
            "修復 clippy 警告（只改以下檔案，cargo check 確認 0 warnings）：\n\
             1. src/multi_agent/mod.rs:21 — 移除 unused import `TeamTask`\n\
             2. src/multi_agent/roles.rs 開頭 — 移除 doc comment 後的空行\n\
             3. src/multi_agent/session.rs 開頭 — 移除 doc comment 後的空行\n\
             4. src/ext_server.rs:26-27 — 修 doc list item overindented\n\
             5. src/diagnose.rs:127 — 移除 format! 巢狀：format!(\"...\", format!(...)) → format!(\"...\", value)"
        );

        let t2 = enqueue(
            "修復 src/researcher/fetch.rs 第 36 行的 clippy error：\n\
             regex 使用了 backreferences（例如 \\\\1 語法），Rust 的 regex crate 不支援。\n\
             請讀取該檔案找到那個正則表達式，改寫成不使用 backreferences 的等效形式。\n\
             改完執行 cargo check 確認 0 errors 再回報。"
        );

        let t3 = enqueue(
            "在 src/ui_egui/workspace.rs 的 show_overview() 函數加任務狀態篩選：\n\
             1. WorkspaceState 加欄位 overview_filter: usize（預設 0）\n\
             2. 在任務列表上方加 tab_bar，標籤：[\"全部\", \"執行中\", \"完成\", \"失敗\"]\n\
             3. 篩選邏輯：\n\
                - 0（全部）：顯示全部\n\
                - 1（執行中）：status == \"RUNNING\" || status == \"PENDING\"\n\
                - 2（完成）：status == \"DONE\"\n\
                - 3（失敗）：status == \"FAILED\" || status == \"ERROR\"\n\
             4. 只改 src/ui_egui/workspace.rs，cargo check 確認 0 errors"
        );

        println!("✅ 已推入 3 個任務：");
        println!("  任務 1 (Clippy warnings): {t1}");
        println!("  任務 2 (Regex fix):       {t2}");
        println!("  任務 3 (Overview filter): {t3}");

        let all = list_all();
        let queued = all.iter().filter(|t| t.status == TaskStatus::Queued).count();
        println!("  目前佇列：{} 筆任務，其中 {queued} 筆 Queued", all.len());
        println!("\n現在請啟動 Worker（透過 MCP 呼叫 agent_start_worker）讓小隊開始工作。");
    }
}
