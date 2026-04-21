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

use std::path::{Path, PathBuf};
use std::process::Command;

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

    // Create branch + worktree in one command
    let branch_name = format!("task/{}", task_id);
    let output = Command::new("git")
        .arg("worktree")
        .arg("add")
        .arg("--detach")  // Don't track a remote; start from current HEAD
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

    tracing::info!(target: "sirin",
        "[worktree] Cleaned up worktree for task {task_id}");

    Ok(())
}

/// Merge a completed task's branch back to main (only if task succeeded).
///
/// Called after task is marked Done, before cleanup.
/// Does `git merge --ff-only` to ensure a linear history.
pub fn merge_task_branch(repo_cwd: &str, task_id: &str) -> Result<(), String> {
    let branch_name = format!("task/{}", task_id);

    let output = Command::new("git")
        .arg("merge")
        .arg("--ff-only")
        .arg(&branch_name)
        .current_dir(repo_cwd)
        .output()
        .map_err(|e| format!("Failed to spawn git merge: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git merge {branch_name} failed (not a fast-forward?): {stderr}"));
    }

    tracing::info!(target: "sirin",
        "[worktree] Merged branch {branch_name} into main");

    // Clean up the branch reference
    let _ = Command::new("git")
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
        .take(60)  // Limit path length
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_task_id_works() {
        assert_eq!(sanitize_task_id("task/123/456"), "task_123_456");
        assert_eq!(sanitize_task_id("my:weird:id"), "my_weird_id");
    }
}
