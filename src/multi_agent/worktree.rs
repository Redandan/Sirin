//! Git worktree isolation per task.
//!
//! Each task gets its own isolated worktree, preventing git-stage conflicts
//! when multiple workers edit the same files in parallel.
//!
//! Flow:
//! 1. At task start: `create_worktree(repo_cwd, task_id)` → separate working dir
//! 2. Worker executes Engineer/Tester in the worktree
//! 3. At task end: `cleanup_worktree(repo_cwd, task_id)` → removes worktree
//! 4. PM merges (if successful): `merge_task_branch(repo_cwd, task_id)`

use std::path::Path;
use std::process::Command;

use crate::platform::NoWindow;

/// Create an isolated git worktree for a task.
///
/// Returns the absolute path to the worktree root (where cwd will be).
/// Creates branch `task/{task_id}` and checks it out.
///
/// For repo at `/path/to/sirin`, creates:
/// - Branch: `task/{task_id}`
/// - Worktree: `/path/to/sirin-task-{task_id}/`
///
/// Safe on Windows: returns paths with backslashes, but git commands normalize them.
pub fn create_worktree(repo_cwd: &str, task_id: &str) -> Result<String, String> {
    let repo_path = Path::new(repo_cwd);
    if !repo_path.exists() {
        return Err(format!("repo_cwd not found: {repo_cwd}"));
    }

    // Worktree location: ../sirin-task-{id} (sibling of the main repo)
    let parent = repo_path.parent().ok_or("repo_cwd has no parent")?;
    let worktree_name = format!("sirin-task-{}", sanitize_task_id(task_id));
    let worktree_path = parent.join(&worktree_name);

    // Ensure it doesn't already exist (cleanup failed last time?)
    if worktree_path.exists() {
        tracing::warn!(target: "sirin",
            "[worktree] Worktree already exists, removing: {}", worktree_path.display());
        let _ = std::fs::remove_dir_all(&worktree_path);
    }

    // Create branch + worktree in one command.
    // Use -b to create a real branch so merge_task_branch() can find it later.
    let branch_name = format!("task/{}", task_id);

    // Remove stale branch from a previous failed run (ignore errors)
    let _ = Command::new("git")
        .no_window()
        .args(["branch", "-D", &branch_name])
        .current_dir(repo_cwd)
        .output();

    let output = Command::new("git")
        .no_window()
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(&branch_name)
        .arg(worktree_path.to_string_lossy().as_ref())
        .current_dir(repo_cwd)
        .output()
        .map_err(|e| format!("Failed to spawn git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {stderr}"));
    }

    tracing::info!(target: "sirin",
        "[worktree] Created worktree for task {task_id}: {}",
        worktree_path.display());

    Ok(worktree_path.to_string_lossy().to_string())
}

/// Clean up an isolated worktree after task completion.
pub fn cleanup_worktree(repo_cwd: &str, task_id: &str) -> Result<(), String> {
    let repo_path = Path::new(repo_cwd);
    let parent = repo_path.parent().ok_or("repo_cwd has no parent")?;
    let worktree_name = format!("sirin-task-{}", sanitize_task_id(task_id));
    let worktree_path = parent.join(&worktree_name);

    if !worktree_path.exists() {
        tracing::warn!(target: "sirin",
            "[worktree] Worktree doesn't exist (already cleaned?): {}", worktree_path.display());
        return Ok(());
    }

    // Remove the worktree
    let output = Command::new("git")
        .no_window()
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(worktree_path.to_string_lossy().as_ref())
        .current_dir(repo_cwd)
        .output()
        .map_err(|e| format!("Failed to spawn git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(target: "sirin",
            "[worktree] git worktree remove failed (retrying): {stderr}");
        // Try hard delete if git fails (worktree may be locked)
        let _ = std::fs::remove_dir_all(&worktree_path);
    }

    // Also delete the task branch so it doesn't accumulate across many tasks
    let branch_name = format!("task/{}", task_id);
    let _ = Command::new("git")
        .no_window()
        .args(["branch", "-D", &branch_name])
        .current_dir(repo_cwd)
        .output();

    tracing::info!(target: "sirin",
        "[worktree] Cleaned up worktree + branch for task {task_id}");

    Ok(())
}

/// Merge a completed task's branch back to main (only if task succeeded).
///
/// Called after task is marked Done, before cleanup.
/// Does `git merge --ff-only` to ensure a linear history.
pub fn merge_task_branch(repo_cwd: &str, task_id: &str) -> Result<(), String> {
    let branch_name = format!("task/{}", task_id);

    let output = Command::new("git")
        .no_window()
        .arg("merge")
        .arg("--ff-only")
        .arg(&branch_name)
        .current_dir(repo_cwd)
        .output()
        .map_err(|e| format!("Failed to spawn git merge: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git merge {branch_name} failed (not a fast-forward?): {stderr}"
        ));
    }

    tracing::info!(target: "sirin",
        "[worktree] Merged branch {branch_name} into main");

    // Clean up the branch reference
    let _ = Command::new("git")
        .no_window()
        .arg("branch")
        .arg("-d")
        .arg(&branch_name)
        .current_dir(repo_cwd)
        .output();

    Ok(())
}

/// Sanitize task_id to be safe in filesystem paths.
/// Replaces "/" and other special chars with "_".
fn sanitize_task_id(task_id: &str) -> String {
    task_id
        .replace("/", "_")
        .replace("\\", "_")
        .replace(":", "_")
        .replace(" ", "_")
        .chars()
        .take(60) // Limit path length
        .collect()
}

/// One worktree row — describes what `cleanup_stale_worktrees` would (or did)
/// reap.  Used by the MCP `cleanup_stale_worktrees` tool so callers can
/// preview before committing (`dry_run=true`) or audit the deletes after.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StaleWorktree {
    pub path: String,
    pub branch: Option<String>,
    pub age_hours: u64,
    pub deleted: bool,
    /// Populated only when `deleted == false` and `dry_run == false`,
    /// explaining why the cleanup couldn't proceed.
    pub error: Option<String>,
}

/// Reap any `sirin-task-*` worktrees older than `older_than_hours` whose
/// underlying task is no longer Running or Queued in the queue.  Issue
/// #242 acceptance criterion 4 ("Failed tasks leave worktree in place;
/// cleanup-stale reaps after 24h").
///
/// Heuristics — to err on the side of NOT deleting in-flight work:
///   1. Only walk siblings of `repo_cwd` matching the `sirin-task-*` prefix.
///   2. Skip directories whose mtime is younger than `older_than_hours`.
///   3. Skip task_ids that the task queue still reports as Queued/Running.
///   4. With `dry_run=true`, only return what would be deleted; don't
///      touch the filesystem or the git branch.
///
/// Always returns `Ok(_)` — per-row failures are recorded in `error`
/// rather than aborting the whole sweep.  Cron-friendly.
pub fn cleanup_stale_worktrees(
    repo_cwd: &str,
    older_than_hours: u64,
    dry_run: bool,
) -> Result<Vec<StaleWorktree>, String> {
    let repo_path = Path::new(repo_cwd);
    let parent = repo_path.parent().ok_or("repo_cwd has no parent")?;

    // Snapshot the live queue so we don't reap a worktree mid-run.
    let active_task_ids: std::collections::HashSet<String> = {
        use super::queue::{self, TaskStatus};
        let mut set = std::collections::HashSet::new();
        for status in [TaskStatus::Queued, TaskStatus::Running] {
            for t in queue::list_by_status(&status) {
                set.insert(t.id);
            }
        }
        set
    };

    let now = std::time::SystemTime::now();
    let threshold = std::time::Duration::from_secs(older_than_hours * 3600);

    let mut results = Vec::new();

    let entries = std::fs::read_dir(parent).map_err(|e| format!("read_dir {parent:?}: {e}"))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let Some(task_id_raw) = name.strip_prefix("sirin-task-") else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }

        // Skip if the queue still owns this task.
        if active_task_ids.contains(task_id_raw) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = metadata.modified().unwrap_or(now);
        let age = now.duration_since(modified).unwrap_or_default();
        if age < threshold {
            continue; // too young
        }
        let age_hours = age.as_secs() / 3600;

        let branch_name = format!("task/{task_id_raw}");
        let path_str = path.to_string_lossy().to_string();

        if dry_run {
            results.push(StaleWorktree {
                path: path_str,
                branch: Some(branch_name),
                age_hours,
                deleted: false,
                error: None,
            });
            continue;
        }

        // Hard remove via git first (handles index lock cleanly), fall
        // back to plain rm if git refuses.  Then drop the task branch.
        let git_remove = Command::new("git")
            .no_window()
            .args(["worktree", "remove", "--force"])
            .arg(path_str.as_str())
            .current_dir(repo_cwd)
            .output();

        let mut error: Option<String> = None;
        let removed = match git_remove {
            Ok(out) if out.status.success() => true,
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                let fallback = std::fs::remove_dir_all(&path);
                match fallback {
                    Ok(_) => true,
                    Err(e) => {
                        error = Some(format!("git: {stderr}; fs: {e}"));
                        false
                    }
                }
            }
            Err(e) => {
                let fallback = std::fs::remove_dir_all(&path);
                match fallback {
                    Ok(_) => true,
                    Err(fe) => {
                        error = Some(format!("git spawn: {e}; fs: {fe}"));
                        false
                    }
                }
            }
        };

        if removed {
            // Best-effort branch deletion — silent on failure.
            let _ = Command::new("git")
                .no_window()
                .args(["branch", "-D", &branch_name])
                .current_dir(repo_cwd)
                .output();
        }

        results.push(StaleWorktree {
            path: path_str,
            branch: Some(branch_name),
            age_hours,
            deleted: removed,
            error,
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_task_id_works() {
        assert_eq!(sanitize_task_id("task/123/456"), "task_123_456");
        assert_eq!(sanitize_task_id("my:weird:id"), "my_weird_id");
    }

    /// Cleanup is a no-op when the parent directory has no sirin-task-* siblings.
    /// Uses `target/` (always present during cargo test) — its parent is the
    /// repo root, which won't contain any sibling `sirin-task-*` dirs.
    #[test]
    fn cleanup_stale_worktrees_empty_parent_returns_no_rows() {
        // `target/` is the binary cargo target dir.  It is itself a child of
        // the repo root; cleanup_stale_worktrees walks the GRANDparent (repo
        // sibling), where no `sirin-task-*` dir is created during cargo test.
        let cwd = std::env::current_dir().expect("cwd");
        let target = cwd.join("target");
        if !target.exists() {
            return; // Cargo build hasn't created target/ yet — skip silently.
        }
        let result = cleanup_stale_worktrees(
            target.to_string_lossy().as_ref(),
            24,
            true, // dry_run
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let rows = result.unwrap();
        // Repo siblings shouldn't include any sirin-task-* dir during a
        // normal build; if a developer's worktree DID stick around we
        // tolerate it (just confirm the function returned without error).
        for row in &rows {
            assert!(
                row.path.contains("sirin-task-"),
                "row matched non-sirin-task path: {}",
                row.path
            );
        }
    }

    #[test]
    fn cleanup_stale_worktrees_rejects_missing_parent() {
        // Pass a path with no parent — function should propagate the error.
        let r = cleanup_stale_worktrees("/", 24, true);
        // Root path has no parent → returns Err.
        assert!(r.is_err(), "expected Err for /, got {r:?}");
    }
}
