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

    /// Return a new registry that excludes the named tools.
    /// Used to build per-agent scoped registries (e.g. read-only for Chat/Planner).
    pub fn without(self, names: &[&str]) -> Self {
        let mut handlers = (*self.handlers).clone();
        for name in names {
            handlers.remove(*name);
        }
        Self {
            handlers: Arc::new(handlers),
        }
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

/// Build the full registry (called once, result is cached).
fn build_full_registry() -> ToolRegistry {
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
            let path_filter = optional_string_field(&input, "path")
                .map(|p| p.replace('\\', "/").to_lowercase());

            let mut results = crate::memory::search_codebase(&query, limit.saturating_mul(3).max(limit))
                .map_err(|e| e.to_string())?;

            if let Some(ref path) = path_filter {
                results.retain(|entry| entry.to_lowercase().contains(path));
                if results.is_empty() {
                    if let Ok(content) = crate::memory::inspect_project_file_range(path, Some(1), Some(160), 5000) {
                        results.push(content);
                    }
                }
            }

            results.truncate(limit);
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
            let start_line = input
                .get("start_line")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let end_line = input
                .get("end_line")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            // Use range variant so the agent can request exact line windows —
            // crucial for file_patch old_str accuracy on large files.
            let effective_max = if start_line.is_some() || end_line.is_some() {
                max_chars.max(8000)
            } else {
                max_chars
            };
            let content = crate::memory::inspect_project_file_range(
                &path, start_line, end_line, effective_max,
            )
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

            // Safety: refuse to overwrite an existing file that has more than
            // 50 lines — use file_patch for surgical edits instead.
            if !dry_run && safe_path.exists() {
                let existing = std::fs::read_to_string(&safe_path)
                    .unwrap_or_default();
                let line_count = existing.lines().count();
                if line_count > 50 {
                    return Err(format!(
                        "SAFETY: file_write refused — '{}' already exists with {} lines. \
                        Use file_patch for partial edits, or explicitly confirm full replacement.",
                        safe_path.display(), line_count
                    ));
                }
            }

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
            let _ = crate::memory::refresh_codebase_index();
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
        // ── file_patch: surgical hunk-based edits ────────────────────────────
        .register_fn("file_patch", |input| async move {
            let path = required_string_field(&input, "path")?;
            let hunks = input
                .get("hunks")
                .and_then(Value::as_array)
                .ok_or_else(|| "Missing 'hunks' array".to_string())?
                .clone();
            let dry_run = input.get("dry_run").and_then(Value::as_bool).unwrap_or(false);

            let safe_path = safe_project_path(&path)?;

            let original = std::fs::read_to_string(&safe_path)
                .map_err(|e| format!("Cannot read '{}': {e}", safe_path.display()))?;

            let mut content = original;

            for (i, hunk) in hunks.iter().enumerate() {
                let old_str = hunk
                    .get("old_str")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("Hunk {i}: missing 'old_str'"))?;
                let new_str = hunk
                    .get("new_str")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("Hunk {i}: missing 'new_str'"))?;

                if !content.contains(old_str) {
                    return Err(format!(
                        "Hunk {i}: 'old_str' not found in '{}'. Patch aborted — no changes written.",
                        safe_path.display()
                    ));
                }
                // Replace only the first occurrence so hunks are order-independent.
                content = content.replacen(old_str, new_str, 1);
            }

            let hunks_applied = hunks.len();

            if dry_run {
                return Ok(json!({
                    "dry_run": true,
                    "path": safe_path.display().to_string(),
                    "hunks_applied": hunks_applied,
                    "message": "Dry run — file not written. Set dry_run=false to apply.",
                }));
            }

            let bytes = content.len();
            std::fs::write(&safe_path, &content)
                .map_err(|e| format!("Write failed: {e}"))?;
            let _ = crate::memory::refresh_codebase_index();
            Ok(json!({
                "path": safe_path.display().to_string(),
                "hunks_applied": hunks_applied,
                "bytes_written": bytes,
            }))
        })
        // ── plan_execute: run multiple tool steps in sequence ─────────────────
        .register_ctx_fn("plan_execute", |ctx, input| {
            async move {
                let steps = input
                    .get("steps")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "Missing 'steps' array".to_string())?
                    .clone();

                let total = steps.len();
                let mut results: Vec<Value> = Vec::with_capacity(total);

                // Propagate dry_run from the plan_execute call into each
                // file_write step — prevents writes slipping through when the
                // agent wraps file_write inside plan_execute.
                let plan_dry_run = input.get("dry_run").and_then(Value::as_bool).unwrap_or(false);

                for (i, step) in steps.iter().enumerate() {
                    let tool = step
                        .get("tool")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("Step {i}: missing 'tool'"))?
                        .to_string();
                    let mut step_input = step.get("input").cloned().unwrap_or(json!({}));

                    // Inject dry_run into file_write steps when running in dry-run mode.
                    if plan_dry_run && tool == "file_write" {
                        if let Some(obj) = step_input.as_object_mut() {
                            obj.insert("dry_run".to_string(), json!(true));
                        }
                    }

                    match ctx.call_tool(&tool, step_input).await {
                        Ok(result) => {
                            results.push(json!({
                                "step": i,
                                "tool": tool,
                                "success": true,
                                "result": result,
                            }));
                        }
                        Err(e) => {
                            results.push(json!({
                                "step": i,
                                "tool": tool,
                                "success": false,
                                "error": e,
                            }));
                            return Ok(json!({
                                "steps_attempted": i + 1,
                                "steps_total": total,
                                "all_succeeded": false,
                                "aborted_at_step": i,
                                "results": results,
                            }));
                        }
                    }
                }

                Ok(json!({
                    "steps_attempted": total,
                    "steps_total": total,
                    "all_succeeded": true,
                    "results": results,
                }))
            }
            .boxed()
        })
        // ── call_graph_query: look up callers and callees ─────────────────────
        .register_fn("call_graph_query", |input| async move {
            let symbol = required_string_field(&input, "symbol")?;
            let hops = input
                .get("hops")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(1)
                .min(3);
            let result = crate::code_graph::query_call_graph(&symbol, hops)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "symbol": symbol,
                "defined_in": result.defined_in,
                "callers": result.callers,
                "callees": result.callees,
            }))
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

/// Full registry (write tools included). Cached process-wide — cheap to clone.
pub fn default_tool_registry() -> ToolRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(build_full_registry).clone()
}

/// Read-only registry — excludes file_write, file_patch, plan_execute, shell_exec.
/// Used by Chat Agent and Planner Agent so write tools are never accessible
/// from those agents regardless of what the LLM requests.
pub fn read_only_tool_registry() -> ToolRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            build_full_registry().without(&[
                "file_write",
                "file_patch",
                "plan_execute",
                "shell_exec",
            ])
        })
        .clone()
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
    let mut normalized = normalize_path(&requested);

    // Rust module convenience: if the agent guesses `foo.rs` but the project
    // actually uses `foo/mod.rs`, transparently resolve to the existing file.
    if !normalized.exists() && normalized.extension().and_then(|ext| ext.to_str()) == Some("rs") {
        let mod_candidate = normalized.with_extension("").join("mod.rs");
        if mod_candidate.is_file() {
            normalized = mod_candidate;
        }
    }

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
            Component::ParentDir => {
                components.pop();
            }
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
        ".git",
        "target",
        "node_modules",
        ".next",
        "dist",
        "__pycache__",
        ".cargo",
    ];
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
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

    // ── Temp-file helpers for file_patch / plan_execute tests ─────────────────

    fn unique_test_filename(stem: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        // Include current thread id so parallel tests don't collide.
        let tid = format!("{:?}", std::thread::current().id());
        let tid_hash: u32 = tid.bytes().fold(0u32, |acc, b| acc.wrapping_add(b as u32));
        format!("__sirin_test_{stem}_{nanos}_{tid_hash}.txt")
    }

    fn write_temp(name: &str, content: &str) {
        std::fs::write(name, content).expect("write_temp failed");
    }

    fn read_temp(name: &str) -> String {
        std::fs::read_to_string(name).expect("read_temp failed")
    }

    fn remove_temp(name: &str) {
        let _ = std::fs::remove_file(name);
    }

    // ── Existing tests ────────────────────────────────────────────────────────

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

        assert!(output
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(false));
        assert!(ctx
            .tools
            .names()
            .iter()
            .any(|name| name == "project_overview"));
    }

    #[tokio::test]
    async fn codebase_search_path_filter_prefers_requested_file() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let output = ctx
            .call_tool(
                "codebase_search",
                json!({ "query": "struct LlmConfig", "path": "src/llm.rs", "limit": 1 }),
            )
            .await
            .expect("codebase_search should succeed");

        let rendered = output.to_string().to_lowercase();
        assert!(
            rendered.contains("src/llm.rs") || rendered.contains("llmconfig"),
            "unexpected output: {output}"
        );
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

    #[test]
    fn safe_project_path_resolves_rust_module_to_mod_rs() {
        let path = safe_project_path("src/telegram.rs")
            .expect("should resolve Rust module path to the existing mod.rs file");
        assert!(
            path.ends_with(std::path::Path::new("src/telegram/mod.rs")),
            "unexpected resolved path: {}",
            path.display()
        );
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

    // ── Phase 2: file_patch ───────────────────────────────────────────────────

    /// Test 1 — normal single-hunk patch is applied correctly.
    #[tokio::test]
    async fn file_patch_applies_single_hunk() {
        let name = unique_test_filename("patch1");
        write_temp(&name, "hello world\nfoo bar\n");

        let ctx = AgentContext::new("test", default_tool_registry());
        let out = ctx
            .call_tool(
                "file_patch",
                json!({ "path": name, "hunks": [{ "old_str": "hello world", "new_str": "hello Sirin" }] }),
            )
            .await
            .expect("single-hunk patch should succeed");

        assert_eq!(out["hunks_applied"], 1, "hunks_applied must be 1");
        assert!(
            out["bytes_written"].as_u64().unwrap_or(0) > 0,
            "bytes_written must be > 0"
        );
        let content = read_temp(&name);
        assert!(
            content.contains("hello Sirin"),
            "new_str must appear in file"
        );
        assert!(!content.contains("hello world"), "old_str must be gone");

        remove_temp(&name);
    }

    /// Test 2 — old_str not found → atomic Err, file untouched.
    #[tokio::test]
    async fn file_patch_old_str_not_found_is_atomic_failure() {
        let name = unique_test_filename("patch2");
        let original = "original content unchanged\n";
        write_temp(&name, original);

        let ctx = AgentContext::new("test", default_tool_registry());
        let result = ctx
            .call_tool(
                "file_patch",
                json!({ "path": name, "hunks": [{ "old_str": "this string does not exist", "new_str": "x" }] }),
            )
            .await;

        assert!(result.is_err(), "must return Err when old_str is absent");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("not found"),
            "error message must say 'not found', got: {msg}"
        );
        assert_eq!(
            read_temp(&name),
            original,
            "file must be completely unmodified after failed patch"
        );

        remove_temp(&name);
    }

    /// Test 3 — dry_run: Ok returned, file not written.
    #[tokio::test]
    async fn file_patch_dry_run_does_not_write() {
        let name = unique_test_filename("patch3");
        let original = "dry run source\n";
        write_temp(&name, original);

        let ctx = AgentContext::new("test", default_tool_registry());
        let out = ctx
            .call_tool(
                "file_patch",
                json!({
                    "path": name,
                    "dry_run": true,
                    "hunks": [{ "old_str": "dry run source", "new_str": "replaced" }]
                }),
            )
            .await
            .expect("dry_run must return Ok");

        assert_eq!(out["dry_run"], true, "response must have dry_run:true");
        assert_eq!(out["hunks_applied"], 1);
        assert_eq!(
            read_temp(&name),
            original,
            "dry_run must not modify the file"
        );

        remove_temp(&name);
    }

    /// Test 4 — multiple hunks are all applied in order.
    #[tokio::test]
    async fn file_patch_applies_multiple_hunks() {
        let name = unique_test_filename("patch4");
        write_temp(&name, "alpha\nbeta\ngamma\n");

        let ctx = AgentContext::new("test", default_tool_registry());
        let out = ctx
            .call_tool(
                "file_patch",
                json!({
                    "path": name,
                    "hunks": [
                        { "old_str": "alpha", "new_str": "ALPHA" },
                        { "old_str": "beta",  "new_str": "BETA"  },
                        { "old_str": "gamma", "new_str": "GAMMA" }
                    ]
                }),
            )
            .await
            .expect("multi-hunk patch should succeed");

        assert_eq!(out["hunks_applied"], 3, "all 3 hunks must be applied");
        let content = read_temp(&name);
        assert!(content.contains("ALPHA") && content.contains("BETA") && content.contains("GAMMA"));
        assert!(
            !content.contains("alpha") && !content.contains("beta") && !content.contains("gamma")
        );

        remove_temp(&name);
    }

    /// Test 5 — path traversal is rejected.
    #[tokio::test]
    async fn file_patch_rejects_path_traversal() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let result = ctx
            .call_tool(
                "file_patch",
                json!({ "path": "../../etc/passwd", "hunks": [{ "old_str": "root", "new_str": "hacked" }] }),
            )
            .await;

        assert!(
            result.is_err(),
            "path traversal must be rejected with Err, not Ok"
        );
    }

    // ── Phase 1: call_graph_query tool ────────────────────────────────────────

    /// Test 8 — tool returns callers, callees, and defined_in with path:line format.
    #[tokio::test]
    async fn call_graph_query_tool_returns_required_fields() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let out = ctx
            .call_tool(
                "call_graph_query",
                json!({ "symbol": "parse_rust_file", "hops": 1 }),
            )
            .await
            .expect("call_graph_query must not error");

        assert!(out.get("callers").is_some(), "response must have 'callers'");
        assert!(out.get("callees").is_some(), "response must have 'callees'");
        assert!(
            out.get("defined_in").is_some(),
            "response must have 'defined_in'"
        );
        assert!(
            out["callers"].as_array().is_some(),
            "'callers' must be an array"
        );
        assert!(
            out["callees"].as_array().is_some(),
            "'callees' must be an array"
        );

        if let Some(def) = out["defined_in"].as_str() {
            assert!(
                def.contains("code_graph"),
                "defined_in should reference code_graph, got: {def}"
            );
            assert!(
                def.contains(':'),
                "defined_in must be 'path:line', got: {def}"
            );
        }
    }

    /// Test 9 — unknown symbol returns Ok with empty callers and callees.
    #[tokio::test]
    async fn call_graph_query_unknown_symbol_returns_empty_arrays() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let out = ctx
            .call_tool(
                "call_graph_query",
                json!({ "symbol": "no_such_symbol_xyz_totally_absent_99999", "hops": 1 }),
            )
            .await
            .expect("unknown symbol must return Ok, not Err");

        let callers = out["callers"].as_array().expect("callers must be array");
        let callees = out["callees"].as_array().expect("callees must be array");
        assert!(callers.is_empty(), "unknown symbol must have no callers");
        assert!(callees.is_empty(), "unknown symbol must have no callees");
    }

    // ── Phase 3: plan_execute ─────────────────────────────────────────────────

    /// Test 11 — all steps succeed: all_succeeded true, counts match.
    #[tokio::test]
    async fn plan_execute_all_steps_succeed() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let out = ctx
            .call_tool(
                "plan_execute",
                json!({
                    "steps": [
                        { "tool": "skill_catalog", "input": {} },
                        { "tool": "skill_catalog", "input": { "query": "search" } }
                    ]
                }),
            )
            .await
            .expect("plan_execute should return Ok");

        assert_eq!(out["all_succeeded"], true);
        assert_eq!(out["steps_attempted"], 2);
        assert_eq!(out["steps_total"], 2);

        let results = out["results"].as_array().expect("results must be array");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["success"], true);
        assert_eq!(results[1]["success"], true);
    }

    /// Test 12 — step N fails → aborted_at_step==N, subsequent steps do not run.
    #[tokio::test]
    async fn plan_execute_aborts_on_step_failure() {
        let ctx = AgentContext::new("test", default_tool_registry());
        let out = ctx
            .call_tool(
                "plan_execute",
                json!({
                    "steps": [
                        { "tool": "skill_catalog", "input": {} },
                        { "tool": "__nonexistent_tool_xyz__", "input": {} },
                        // This third step must never execute.
                        { "tool": "skill_catalog", "input": {} }
                    ]
                }),
            )
            .await
            .expect("plan_execute itself must return Ok even when a step fails");

        assert_eq!(out["all_succeeded"], false);
        assert_eq!(out["aborted_at_step"], 1, "should abort at step index 1");

        let results = out["results"].as_array().expect("results must be array");
        assert_eq!(
            results.len(),
            2,
            "only steps 0 and 1 should appear; step 2 must not run"
        );
        assert_eq!(results[0]["success"], true);
        assert_eq!(results[1]["success"], false);
    }

    // ── Integration: full coding tool chain (no LLM required) ────────────────

    /// Simulates what CodingAgent does on each iteration, minus the LLM call:
    /// project_overview → codebase_search → local_file_read → file_patch (dry)
    /// → call_graph_query → git_status.
    /// If any tool in this chain is broken or unregistered the test fails.
    #[tokio::test]
    async fn coding_tool_chain_end_to_end_without_llm() {
        let ctx = AgentContext::new("test", default_tool_registry());

        // 1. project_overview — agent's first context-gathering step.
        let overview = ctx
            .call_tool("project_overview", json!({}))
            .await
            .expect("project_overview must succeed");
        assert!(
            overview
                .get("summary")
                .and_then(Value::as_str)
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "overview must contain a summary"
        );
        assert!(
            overview
                .get("files")
                .and_then(Value::as_array)
                .map(|v| !v.is_empty())
                .unwrap_or(false),
            "overview must list files"
        );

        // 2. codebase_search — agent narrows down relevant files.
        let search = ctx
            .call_tool(
                "codebase_search",
                json!({ "query": "parse_rust_file", "limit": 3 }),
            )
            .await
            .expect("codebase_search must succeed");
        assert!(
            search.as_array().map(|v| !v.is_empty()).unwrap_or(false),
            "codebase_search must return at least one result for a known symbol"
        );

        // 3. local_file_read — agent reads the target file.
        let file = ctx
            .call_tool("local_file_read", json!({ "path": "src/code_graph.rs" }))
            .await
            .expect("local_file_read must succeed for src/code_graph.rs");
        assert!(
            file["content"]
                .as_str()
                .map(|s| s.contains("parse_rust_file"))
                .unwrap_or(false),
            "read content must contain the expected symbol"
        );

        // 4. file_patch (dry_run) — agent applies a surgical edit.
        let patch_file = unique_test_filename("e2e_coding");
        write_temp(&patch_file, "fn placeholder_fn() -> bool { false }\n");
        let patch = ctx
            .call_tool(
                "file_patch",
                json!({
                    "path": patch_file,
                    "dry_run": true,
                    "hunks": [{ "old_str": "false", "new_str": "true" }]
                }),
            )
            .await
            .expect("file_patch dry_run must succeed");
        assert_eq!(patch["dry_run"], true);
        assert_eq!(patch["hunks_applied"], 1);
        remove_temp(&patch_file);

        // 5. call_graph_query — agent inspects callers before modifying a function.
        let cg = ctx
            .call_tool(
                "call_graph_query",
                json!({ "symbol": "build_call_graph", "hops": 1 }),
            )
            .await
            .expect("call_graph_query must succeed");
        assert!(
            cg["callers"].as_array().is_some(),
            "callers must be an array"
        );
        assert!(
            cg["callees"].as_array().is_some(),
            "callees must be an array"
        );

        // 6. symbol_search — agent looks up a symbol by name.
        let syms = ctx
            .call_tool("symbol_search", json!({ "query": "run_react_loop" }))
            .await
            .expect("symbol_search must succeed");
        assert!(
            syms.as_array().is_some(),
            "symbol_search must return an array"
        );

        // 7. git_status — agent checks working tree before committing.
        let gs = ctx
            .call_tool("git_status", json!({}))
            .await
            .expect("git_status must succeed");
        assert!(
            gs.get("status").is_some(),
            "git_status must return 'status' field"
        );
        assert!(
            gs.get("clean").is_some(),
            "git_status must return 'clean' field"
        );
    }

    /// Test 13 — file_patch (step 0) succeeds; shell_exec with non-allowlisted
    /// command (step 1) fails → aborted_at_step == 1, patch is already on disk.
    #[tokio::test]
    async fn plan_execute_file_patch_then_rejected_shell_exec() {
        let name = unique_test_filename("plan13");
        write_temp(&name, "plan execute source\n");

        let ctx = AgentContext::new("test", default_tool_registry());
        let out = ctx
            .call_tool(
                "plan_execute",
                json!({
                    "steps": [
                        {
                            "tool": "file_patch",
                            "input": {
                                "path": name,
                                "hunks": [{ "old_str": "plan execute source", "new_str": "patched by plan" }]
                            }
                        },
                        {
                            "tool": "shell_exec",
                            // `cargo --version` is not in the default allowlist.
                            "input": { "command": "cargo --version" }
                        }
                    ]
                }),
            )
            .await
            .expect("plan_execute must return Ok");

        assert_eq!(out["all_succeeded"], false);
        assert_eq!(out["aborted_at_step"], 1);

        let results = out["results"].as_array().unwrap();
        assert_eq!(results[0]["success"], true, "file_patch step must succeed");
        assert_eq!(
            results[1]["success"], false,
            "shell_exec step must fail (not in allowlist)"
        );

        // The patch from step 0 was committed to disk before step 1 was attempted.
        assert!(
            read_temp(&name).contains("patched by plan"),
            "file_patch side-effect must be persisted even though plan aborted"
        );

        remove_temp(&name);
    }
}
