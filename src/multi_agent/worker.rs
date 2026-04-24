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

use std::collections::HashMap;
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

fn run_loop(default_cwd: String, worker_id: usize) {
    // Per-project team pool — keyed by normalized project_key ("" / "sirin" /
    // "agora_market" / ...). Lazy: each project's team is loaded on first
    // task that references it. Sirin team is preloaded so legacy queues
    // (no `project` field) keep behaving identically.
    let mut teams: HashMap<String, AgentTeam> = HashMap::new();
    teams.insert(
        String::new(),
        AgentTeam::load_for_worker_project(&default_cwd, worker_id, ""),
    );

    loop {
        // 原子操作：取任務同時標記 Running，多 worker 安全
        match queue::take_next_queued() {
            Some(task) => {
                // 安全截斷：找 80 bytes 內最後的 char boundary
                let preview_end = {
                    let max = task.description.len().min(80);
                    (0..=max).rev().find(|&i| task.description.is_char_boundary(i)).unwrap_or(0)
                };

                // Resolve which (project_key, cwd, extra_tools) this task targets.
                // No `project` field on legacy tasks → empty key + default Sirin cwd.
                let (project_key, project_cwd, extra_tools) =
                    resolve_project(&task, &default_cwd);

                tracing::info!(target: "sirin",
                    "[team-worker:w{worker_id}] Starting task {} [{}] — {}",
                    task.id,
                    if project_key.is_empty() { "sirin" } else { project_key.as_str() },
                    &task.description[..preview_end]);

                // Look up (or lazily build) the team for this project.
                let team = teams.entry(project_key.clone()).or_insert_with(|| {
                    AgentTeam::load_for_worker_project(&project_cwd, worker_id, &project_key)
                });
                team.set_extra_tools(&extra_tools);
                let dry_run = task.project.as_ref()
                    .map(|p| p.dry_run).unwrap_or(false);
                team.set_dry_run(dry_run);

                // Capture issue_url before assign_task — task may be moved/cloned.
                let issue_url = task.project.as_ref()
                    .and_then(|p| p.issue_url.clone());

                // T2-2: capture yaml_test_id before assign_task consumes task borrow.
                let yaml_test_id: Option<String> = task.project.as_ref()
                    .and_then(|p| p.yaml_test_id.clone());

                match team.assign_task(&task.description) {
                    Ok(review) => {
                        tracing::info!(target: "sirin",
                            "[team-worker:w{worker_id}] Task {} done ✓", task.id);
                        queue::update_status(&task.id, TaskStatus::Done, Some(review.clone()));

                        // Extract and persist any lessons the PM logged in the review
                        let lessons = super::knowledge::parse_lessons(&review);
                        super::knowledge::store_lessons(&task.id, &lessons);

                        // T2-2: YAML test auto-verification (if requested).
                        // Only run when Sirin has a browser available (test_runner
                        // drives Chrome). Skip on non-Sirin projects for now.
                        if let Some(ref ytid) = yaml_test_id {
                            tracing::info!(target: "sirin",
                                "[team-worker:w{worker_id}] T2-2 yaml_test_cycle: test_id={ytid}");
                            match team.yaml_test_cycle(&default_cwd, ytid) {
                                Ok(summary) => tracing::info!(target: "sirin",
                                    "[team-worker:w{worker_id}] yaml_test_cycle ✓ {summary}"),
                                Err(e) => tracing::warn!(target: "sirin",
                                    "[team-worker:w{worker_id}] yaml_test_cycle ✗ {e}"),
                            }
                        }

                        // 驗證編譯（cargo check）— only for Sirin team; cross-repo
                        // projects don't necessarily have Rust / cargo.
                        if project_key.is_empty() || project_key.eq_ignore_ascii_case("sirin") {
                            if let Err(e) = team.test_cycle() {
                                tracing::warn!(target: "sirin",
                                    "[team-worker:w{worker_id}] test_cycle after task {}: {e}", task.id);
                            }
                        }

                        // Loop closure: post review back to GitHub issue if linked.
                        // dry_run → divert to preview file (no GitHub write).
                        // Non-fatal — gh failure shouldn't crash the worker.
                        if let Some(url) = issue_url.as_deref() {
                            if dry_run {
                                save_preview_comment(&task.id, url, &review, true);
                            } else {
                                post_issue_comment(&task.id, url, &review, true);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "sirin",
                            "[team-worker:w{worker_id}] Task {} failed: {e}", task.id);
                        queue::update_status(&task.id, TaskStatus::Failed, Some(e.clone()));

                        // Same loop closure on failure — surface the error to the
                        // human watching the issue, not just the Sirin UI.
                        if let Some(url) = issue_url.as_deref() {
                            if dry_run {
                                save_preview_comment(&task.id, url, &e, false);
                            } else {
                                post_issue_comment(&task.id, url, &e, false);
                            }
                        }

                        if task.retry_count == 0 {
                            // 安全截斷到 200 bytes 的 char boundary
                            let err_end = {
                                let max = e.len().min(200);
                                (0..=max).rev().find(|&i| e.is_char_boundary(i)).unwrap_or(0)
                            };
                            let retry_desc = format!(
                                "[auto-retry] {}\n\nOriginal failure: {}",
                                task.description, &e[..err_end]
                            );
                            queue::enqueue_with_retry(&retry_desc, 1);
                            tracing::info!(target: "sirin",
                                "[team-worker] Auto-retrying task {} (1st retry)", task.id);
                        } else {
                            tracing::warn!(target: "sirin",
                                "[team-worker] Task {} failed after retry", task.id);
                        }
                    }
                }

                // Always clear extra_tools / dry_run after the task — the next
                // task may run on the same team but must not inherit per-task
                // permissions or guardrails.
                team.set_extra_tools(&[]);
                team.set_dry_run(false);

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

// ── Project routing ───────────────────────────────────────────────────────────

/// DRY-RUN diversion: save the comment that WOULD have been posted to a
/// `preview_comments.jsonl` file so a human can inspect + approve before
/// anything touches GitHub.
///
/// Location: `data/multi_agent/preview_comments.jsonl` (one JSON object per
/// line, same dir as the task queue).
///
/// Each record carries enough context for a human or the `replay_preview`
/// helper in `github_adapter` to re-post manually after review.
///
/// `pub(super)` so integration tests in sibling modules (e.g.
/// `github_adapter::tests`) can replicate the worker's task-completion
/// branch when validating dry-run end-to-end.
pub(super) fn save_preview_comment(task_id: &str, issue_url: &str, body: &str, success: bool) {
    let record = serde_json::json!({
        "task_id":   task_id,
        "issue_url": issue_url,
        "success":   success,
        "body":      body,
        "saved_at":  chrono::Local::now().to_rfc3339(),
    });
    let path = crate::platform::app_data_dir()
        .join("data")
        .join("multi_agent")
        .join("preview_comments.jsonl");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    use std::io::Write;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{record}"));

    match result {
        Ok(_) => tracing::info!(target: "sirin",
            "[team-worker] [DRY-RUN] Comment preview saved for task {task_id} → {}",
            path.display()),
        Err(e) => tracing::warn!(target: "sirin",
            "[team-worker] [DRY-RUN] Failed to save preview for task {task_id}: {e}"),
    }
}

/// Post the task's final review (or error) back to a linked GitHub issue.
///
/// `success=true` → "[Sirin Dev Team ✓ Done]" header
/// `success=false` → "[Sirin Dev Team ✗ Failed]" header
///
/// Failure (gh missing / network down / not authenticated) is logged at
/// `warn` level but never propagates — the queue should keep flowing
/// even if GitHub is unreachable.
fn post_issue_comment(task_id: &str, issue_url: &str, body: &str, success: bool) {
    let header = if success { "✓ Done" } else { "✗ Failed" };
    // Cap body — GitHub comments support 65k chars but readability dies past 4k.
    let body_capped = {
        let max = body.len().min(4_000);
        let end = (0..=max).rev()
            .find(|&i| body.is_char_boundary(i))
            .unwrap_or(0);
        if end < body.len() {
            format!("{}\n\n_…(truncated, full review in Sirin queue: task `{task_id}`)_",
                &body[..end])
        } else {
            body.to_string()
        }
    };
    let comment = format!(
        "**[Sirin Dev Team {header}]**\n\n{body_capped}\n\n\
         <sub>Posted automatically by Sirin AgentTeam worker · task `{task_id}`</sub>",
    );

    match super::github_adapter::comment_on_issue_url(issue_url, &comment) {
        Ok(_) => tracing::info!(target: "sirin",
            "[team-worker] Posted review for task {task_id} to {issue_url}"),
        Err(e) => tracing::warn!(target: "sirin",
            "[team-worker] Failed to post comment for task {task_id} to {issue_url}: {e}"),
    }
}

/// Decide which (project_key, cwd, extra_tools) a task should run under.
///
/// Rules:
/// - No `project` field, empty repo, or `repo == "sirin"` → ("", default_cwd, [])
///   (preserves legacy behaviour for every existing queued task).
/// - Otherwise → resolve repo via `claude_session::repo_path()`. If unknown,
///   fall back to default_cwd but keep the project key for session isolation
///   (so tasks targeting an undefined repo still get their own session_id and
///   don't pollute Sirin's PM/Engineer/Tester history).
fn resolve_project(
    task: &queue::TeamTask,
    default_cwd: &str,
) -> (String, String, Vec<String>) {
    let Some(ctx) = task.project.as_ref() else {
        return (String::new(), default_cwd.to_string(), Vec::new());
    };

    let repo = ctx.repo.trim();
    if repo.is_empty() || repo.eq_ignore_ascii_case("sirin") {
        return (String::new(), default_cwd.to_string(), ctx.extra_tools.clone());
    }

    // Normalize project key (lowercase, ascii) to match session file naming.
    let key = repo.to_ascii_lowercase();
    let cwd = crate::claude_session::repo_path(repo)
        .unwrap_or_else(|| {
            tracing::warn!(target: "sirin",
                "[team-worker] Unknown repo '{repo}' — falling back to default cwd, \
                 but keeping project session namespace");
            default_cwd.to_string()
        });
    (key, cwd, ctx.extra_tools.clone())
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
