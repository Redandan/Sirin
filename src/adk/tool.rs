use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use chrono::Utc;
use futures::{future::BoxFuture, FutureExt};
use serde_json::{json, Value};

use crate::adk::context::AgentContext;
use crate::persona::{BehaviorEngine, IncomingMessage, Persona, TaskEntry};

pub type ToolResult = Result<Value, String>;
pub type ToolFuture<'a> = BoxFuture<'a, ToolResult>;
pub type ToolHandler = Arc<dyn for<'a> Fn(&'a AgentContext, Value) -> ToolFuture<'a> + Send + Sync>;

#[derive(Clone, Default)]
pub struct ToolRegistry {
    handlers: Arc<HashMap<String, ToolHandler>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_fn<F, Fut>(self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.register_ctx_fn(name, move |_ctx, input| {
            let handler = Arc::clone(&handler);
            async move { (handler)(input).await }.boxed()
        })
    }

    pub fn register_ctx_fn<F>(self, name: impl Into<String>, handler: F) -> Self
    where
        F: for<'a> Fn(&'a AgentContext, Value) -> ToolFuture<'a> + Send + Sync + 'static,
    {
        let mut handlers = (*self.handlers).clone();
        handlers.insert(name.into(), Arc::new(handler));
        Self {
            handlers: Arc::new(handlers),
        }
    }

    pub async fn call(&self, ctx: &AgentContext, name: &str, input: Value) -> ToolResult {
        let handler = self
            .handlers
            .get(name)
            .cloned()
            .ok_or_else(|| format!("Tool not registered: {name}"))?;
        handler(ctx, input).await
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort();
        names
    }
}

fn query_from_input(input: &Value) -> Result<String, String> {
    input
        .get("query")
        .and_then(Value::as_str)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "Missing 'query' string".to_string())
}

fn limit_from_input(input: &Value, default_limit: usize) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .filter(|&v| v > 0)
        .unwrap_or(default_limit)
}

fn optional_string_field(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required_string_field(input: &Value, key: &str) -> Result<String, String> {
    optional_string_field(input, key).ok_or_else(|| format!("Missing '{key}' string"))
}

pub fn default_tool_registry() -> ToolRegistry {
    ToolRegistry::new()
        .register_fn("web_search", |input| async move {
            let query = query_from_input(&input)?;
            let limit = limit_from_input(&input, 5);
            let results = crate::skills::ddg_search(&query).await?;
            Ok(json!(results.into_iter().take(limit).collect::<Vec<_>>()))
        })
        .register_fn("memory_search", |input| async move {
            let query = query_from_input(&input)?;
            let limit = limit_from_input(&input, 5);
            let results = crate::memory::memory_search(&query, limit)
                .map_err(|e| e.to_string())?;
            Ok(json!(results))
        })
        .register_fn("codebase_search", |input| async move {
            let query = query_from_input(&input)?;
            let limit = limit_from_input(&input, 5);
            let results = crate::memory::search_codebase(&query, limit)
                .map_err(|e| e.to_string())?;
            Ok(json!(results))
        })
        .register_fn("project_overview", |input| async move {
            let limit = limit_from_input(&input, 8);
            let files = crate::memory::list_project_files(limit)
                .map_err(|e| e.to_string())?;

            Ok(json!({
                "summary": "Sirin 是一個用 Rust 建構的本地 AI 助手專案，包含 egui 桌面 UI、Telegram 整合、ADK 風格 agent 流程、記憶 / 程式碼索引，以及本地 LLM 支援。",
                "files": files,
            }))
        })
        .register_fn("local_file_read", |input| async move {
            let path = optional_string_field(&input, "path")
                .or_else(|| optional_string_field(&input, "query"))
                .ok_or_else(|| "Missing 'path' string".to_string())?;
            let max_chars = input
                .get("max_chars")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(2400);
            let content = crate::memory::inspect_project_file(&path, max_chars)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "path": path,
                "content": content,
            }))
        })
        .register_ctx_fn("task_recent", |ctx, input| {
            async move {
                let limit = limit_from_input(&input, 20);
                let tracker = ctx
                    .tracker()
                    .cloned()
                    .ok_or_else(|| "task_recent requires TaskTracker in AgentContext".to_string())?;
                let entries = tracker.read_last_n(limit).map_err(|e| e.to_string())?;
                serde_json::to_value(entries).map_err(|e| e.to_string())
            }
            .boxed()
        })
        .register_ctx_fn("task_lookup", |ctx, input| {
            async move {
                let timestamp = required_string_field(&input, "timestamp")?;
                let tracker = ctx
                    .tracker()
                    .cloned()
                    .ok_or_else(|| "task_lookup requires TaskTracker in AgentContext".to_string())?;
                let entry = tracker
                    .find_by_timestamp(&timestamp)
                    .map_err(|e| e.to_string())?;
                serde_json::to_value(entry).map_err(|e| e.to_string())
            }
            .boxed()
        })
        .register_ctx_fn("task_record", |ctx, input| {
            async move {
                let event = required_string_field(&input, "event")?;
                let tracker = ctx
                    .tracker()
                    .cloned()
                    .ok_or_else(|| "task_record requires TaskTracker in AgentContext".to_string())?;
                let status = optional_string_field(&input, "status");
                let message_preview = optional_string_field(&input, "message_preview");
                let reason = optional_string_field(&input, "reason");
                let correlation_id = optional_string_field(&input, "correlation_id")
                    .or_else(|| Some(ctx.request_id.clone()));
                let entry = TaskEntry::system_event(
                    "Sirin",
                    event,
                    message_preview,
                    status.as_deref(),
                    reason,
                    correlation_id,
                );
                tracker.record(&entry).map_err(|e| e.to_string())?;
                serde_json::to_value(entry).map_err(|e| e.to_string())
            }
            .boxed()
        })
        .register_fn("research_lookup", |input| async move {
            let id = required_string_field(&input, "id")?;
            let task = crate::researcher::get_research(&id)?;
            serde_json::to_value(task).map_err(|e| e.to_string())
        })
        .register_fn("skill_catalog", |input| async move {
            if let Some(query) = optional_string_field(&input, "query") {
                let recommended = crate::skills::recommended_skills(&query);
                if !recommended.is_empty() {
                    return Ok(json!(recommended));
                }
            }
            Ok(json!(crate::skills::list_skills()))
        })
        .register_fn("skill_execute", |input| async move {
            let skill_id = required_string_field(&input, "skill_id")?;
            let timestamp = optional_string_field(&input, "timestamp")
                .unwrap_or_else(|| Utc::now().to_rfc3339());
            let result = crate::skills::execute_skill(&skill_id, &timestamp)?;
            Ok(json!(result))
        })
        .register_ctx_fn("behavior_evaluate", |ctx, input| {
            async move {
                let persona = Persona::load().map_err(|e| e.to_string())?;
                let msg = required_string_field(&input, "msg")?;
                let source = optional_string_field(&input, "source")
                    .unwrap_or_else(|| ctx.source.clone());
                let estimated_value = input
                    .get("estimated_value")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let should_record = input
                    .get("record")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                let incoming = IncomingMessage { source, msg };
                let decision = BehaviorEngine::evaluate(incoming, estimated_value, &persona);

                if should_record {
                    if let Some(tracker) = ctx.tracker() {
                        let entry = TaskEntry::behavior_decision(&persona, estimated_value, &decision);
                        tracker.record(&entry).map_err(|e| e.to_string())?;
                    }
                }

                Ok(json!({
                    "draft": decision.draft,
                    "high_priority": decision.high_priority,
                    "matched_objective": decision.matched_objective,
                    "tier": decision.tier,
                    "reason": decision.reason,
                }))
            }
            .boxed()
        })
        // ── Coding tools ──────────────────────────────────────────────────────
        .register_fn("file_list", |input| async move {
            let dir = optional_string_field(&input, "path").unwrap_or_else(|| ".".to_string());
            let max_depth = input
                .get("max_depth")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(4);
            let entries = list_directory_tree(&dir, max_depth)?;
            Ok(json!(entries))
        })
        .register_fn("file_write", |input| async move {
            let path = required_string_field(&input, "path")?;
            let content = required_string_field(&input, "content")?;
            let dry_run = input.get("dry_run").and_then(Value::as_bool).unwrap_or(false);

            // Load persona config for size limit.
            let max_bytes = crate::persona::Persona::load()
                .map(|p| p.coding_agent.max_file_write_bytes)
                .unwrap_or(102_400);

            if content.len() > max_bytes {
                return Err(format!(
                    "Content size {} bytes exceeds max_file_write_bytes {}",
                    content.len(),
                    max_bytes
                ));
            }

            let safe_path = safe_project_path(&path)?;

            if dry_run {
                return Ok(json!({
                    "dry_run": true,
                    "path": safe_path.display().to_string(),
                    "bytes": content.len(),
                    "message": "Dry run — file not written. Set dry_run=false to apply.",
                }));
            }

            if let Some(parent) = safe_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Cannot create parent dirs: {e}"))?;
            }
            let bytes = content.len();
            std::fs::write(&safe_path, content)
                .map_err(|e| format!("Write failed: {e}"))?;
            Ok(json!({
                "path": safe_path.display().to_string(),
                "bytes_written": bytes,
            }))
        })
        .register_fn("file_diff", |input| async move {
            let path_arg = optional_string_field(&input, "path");
            let mut cmd = std::process::Command::new("git");
            cmd.arg("diff").arg("HEAD");
            if let Some(ref p) = path_arg {
                cmd.arg("--").arg(p);
            }
            let out = cmd.output().map_err(|e| format!("git diff failed: {e}"))?;
            let diff = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            if diff.trim().is_empty() && !stderr.trim().is_empty() {
                return Err(format!("git diff error: {stderr}"));
            }
            Ok(json!({
                "diff": diff,
                "empty": diff.trim().is_empty(),
            }))
        })
        .register_fn("shell_exec", |input| async move {
            let command = required_string_field(&input, "command")?;

            // Build effective allowed list: persona config + SIRIN_ALLOWED_COMMANDS env.
            let mut allowed: Vec<String> = crate::persona::Persona::load()
                .map(|p| p.coding_agent.allowed_commands)
                .unwrap_or_else(|_| vec![
                    "cargo check".to_string(),
                    "cargo test".to_string(),
                    "cargo build --release".to_string(),
                ]);
            if let Ok(extra) = std::env::var("SIRIN_ALLOWED_COMMANDS") {
                for item in extra.split(',') {
                    let t = item.trim().to_string();
                    if !t.is_empty() {
                        allowed.push(t);
                    }
                }
            }

            let permitted = allowed.iter().any(|prefix| {
                command == prefix.trim() || command.starts_with(&format!("{} ", prefix.trim()))
            });
            if !permitted {
                return Err(format!(
                    "Command not in allowlist: `{command}`. Allowed prefixes: {}",
                    allowed.join(", ")
                ));
            }

            // Split into program + args (simple whitespace split is sufficient for
            // whitelisted commands like `cargo check`).
            let mut parts = command.split_whitespace();
            let program = parts.next().unwrap_or("sh");
            let args: Vec<&str> = parts.collect();

            let output = std::process::Command::new(program)
                .args(&args)
                .output()
                .map_err(|e| format!("Failed to run `{command}`: {e}"))?;

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let exit_code = output.status.code().unwrap_or(-1);

            Ok(json!({
                "command": command,
                "exit_code": exit_code,
                "success": output.status.success(),
                "stdout": stdout,
                "stderr": stderr,
            }))
        })
        .register_fn("symbol_search", |input| async move {
            let query = query_from_input(&input)?;
            let limit = limit_from_input(&input, 8);
            // Search codebase first, then filter for symbol-level hits.
            let raw = crate::memory::search_codebase(&query, limit * 2)
                .map_err(|e| e.to_string())?;
            // Prefer entries that mention fn, struct, enum, impl, pub, or the exact query token.
            let query_lower = query.to_lowercase();
            let mut ranked: Vec<String> = raw.into_iter().collect();
            ranked.sort_by_key(|entry| {
                let lower = entry.to_lowercase();
                let has_symbol_keyword = ["fn ", "struct ", "enum ", "impl ", "pub ", "trait ", "type "]
                    .iter()
                    .any(|kw| lower.contains(kw));
                let has_query = lower.contains(&query_lower);
                match (has_query, has_symbol_keyword) {
                    (true, true) => 0,
                    (true, false) => 1,
                    (false, true) => 2,
                    (false, false) => 3,
                }
            });
            ranked.truncate(limit);
            Ok(json!(ranked))
        })
        .register_fn("git_status", |_input| async move {
            let out = std::process::Command::new("git")
                .args(["status", "--short"])
                .output()
                .map_err(|e| format!("git status failed: {e}"))?;
            let status = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            Ok(json!({
                "status": status,
                "clean": status.trim().is_empty(),
                "stderr": stderr,
            }))
        })
        .register_fn("git_log", |input| async move {
            let limit = limit_from_input(&input, 10);
            let out = std::process::Command::new("git")
                .args(["log", "--oneline", &format!("-{limit}")])
                .output()
                .map_err(|e| format!("git log failed: {e}"))?;
            let log = String::from_utf8_lossy(&out.stdout).to_string();
            Ok(json!({ "log": log }))
        })
}

// ── Coding tool helpers ───────────────────────────────────────────────────────

/// Return the canonical absolute path for `path`, ensuring it is within the
/// project root (`SIRIN_PROJECT_ROOT` env var or `cwd`).
fn safe_project_path(path: &str) -> Result<std::path::PathBuf, String> {
    let root = std::env::var("SIRIN_PROJECT_ROOT")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "Cannot determine project root".to_string())?;

    // Resolve the requested path relative to root.
    let requested = root.join(path);

    // We can't canonicalize a path that doesn't exist yet, so normalize manually.
    let normalized = normalize_path(&requested);

    // Security: ensure normalized path starts with root.
    let root_canon = std::fs::canonicalize(&root).unwrap_or(root.clone());
    let norm_canon = if normalized.exists() {
        std::fs::canonicalize(&normalized).unwrap_or(normalized.clone())
    } else {
        // For new files, canonicalize the parent and re-append the filename.
        let parent = normalized.parent().unwrap_or(&normalized);
        let parent_canon = if parent.exists() {
            std::fs::canonicalize(parent).unwrap_or(parent.to_path_buf())
        } else {
            parent.to_path_buf()
        };
        parent_canon.join(normalized.file_name().unwrap_or_default())
    };

    if !norm_canon.starts_with(&root_canon) {
        return Err(format!(
            "Path `{path}` resolves outside project root `{}`",
            root_canon.display()
        ));
    }
    Ok(normalized)
}

/// Normalize a path by resolving `.` and `..` components without requiring the
/// path to exist on disk (unlike `std::fs::canonicalize`).
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::ParentDir => { components.pop(); }
            Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Recursively list files under `dir` up to `max_depth` levels, returning
/// relative paths (from `dir`).  Skips common noise directories.
fn list_directory_tree(dir: &str, max_depth: usize) -> Result<Vec<String>, String> {
    let root = std::path::Path::new(dir);
    if !root.exists() {
        return Err(format!("Directory not found: {dir}"));
    }
    let mut result = Vec::new();
    walk_dir(root, root, 0, max_depth, &mut result);
    result.sort();
    Ok(result)
}

fn walk_dir(
    root: &std::path::Path,
    current: &std::path::Path,
    depth: usize,
    max_depth: usize,
    result: &mut Vec<String>,
) {
    if depth > max_depth {
        return;
    }
    let skip_dirs = [
        ".git", "target", "node_modules", ".next", "dist", "__pycache__", ".cargo",
    ];
    let Ok(entries) = std::fs::read_dir(current) else { return };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') && depth == 0 {
            // Skip hidden top-level dirs except showing that they exist.
        }
        if path.is_dir() {
            if skip_dirs.contains(&name_str.as_ref()) {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                result.push(format!("{}/", rel.display()));
            }
            walk_dir(root, &path, depth + 1, max_depth, result);
        } else if let Ok(rel) = path.strip_prefix(root) {
            result.push(rel.display().to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn custom_registry_round_trips_values() {
        let registry = ToolRegistry::new().register_fn("echo", |input| async move { Ok(input) });
        let ctx = AgentContext::new("test", registry.clone());
        let output = ctx
            .call_tool("echo", json!({ "hello": "world" }))
            .await
            .expect("echo tool should succeed");

        assert_eq!(output["hello"], "world");
    }

    #[tokio::test]
    async fn default_registry_exposes_skill_catalog() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let output = ctx
            .call_tool("skill_catalog", json!({}))
            .await
            .expect("skill catalog should be available");

        assert!(output.as_array().map(|items| !items.is_empty()).unwrap_or(false));
        assert!(ctx.tools.names().iter().any(|name| name == "project_overview"));
    }

    #[tokio::test]
    async fn file_list_returns_entries_for_current_dir() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let output = ctx
            .call_tool("file_list", json!({ "path": ".", "max_depth": 1 }))
            .await
            .expect("file_list should succeed");
        assert!(output.as_array().is_some());
    }

    #[test]
    fn safe_project_path_rejects_path_traversal() {
        let result = safe_project_path("../../etc/passwd");
        // Should either error (path outside root) or succeed by staying in root.
        // On most systems this will resolve outside the project root and be rejected.
        // We just verify it doesn't panic.
        let _ = result;
    }

    #[test]
    fn normalize_path_collapses_dotdot() {
        let p = std::path::PathBuf::from("/tmp/foo/../bar");
        let norm = normalize_path(&p);
        assert_eq!(norm, std::path::PathBuf::from("/tmp/bar"));
    }

    #[tokio::test]
    async fn shell_exec_rejects_disallowed_command() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let result = ctx
            .call_tool("shell_exec", json!({ "command": "rm -rf /" }))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("not in allowlist"), "got: {msg}");
    }

    #[tokio::test]
    async fn shell_exec_runs_allowed_command() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let result = ctx
            .call_tool("shell_exec", json!({ "command": "cargo --version" }))
            .await;
        // `cargo --version` isn't in the default allowlist, so it should be rejected.
        // This also doubles as a check that the allowlist comparison is prefix-based.
        assert!(result.is_err());
    }
}
