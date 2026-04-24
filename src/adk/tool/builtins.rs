//! Built-in tool registrations.
//!
//! `build_full_registry()` wires up every tool Sirin ships with:
//! search (web/memory/codebase/symbol), filesystem (read/write/patch/list),
//! task tracking, git status/log, shell exec (allowlisted), skills, behaviour
//! evaluation, call graph, web navigation, agent handoff, and plan execution.
//! External MCP tools discovered at startup are appended last.

use futures_util::FutureExt;
use serde_json::{json, Value};

use super::fs_helpers::{list_directory_tree, safe_project_path};
use super::ToolRegistry;
use crate::persona::{BehaviorEngine, IncomingMessage, Persona, TaskEntry};

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
    input.get(key).and_then(|v| {
        // Accept both JSON strings and JSON numbers (LLMs sometimes send numbers
        // for numeric targets like {"target": 2000} instead of {"target": "2000"}).
        let s = match v {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => return None,
        };
        if s.is_empty() { None } else { Some(s) }
    })
}

fn required_string_field(input: &Value, key: &str) -> Result<String, String> {
    optional_string_field(input, key).ok_or_else(|| format!("Missing '{key}' string"))
}

/// Register all discovered external MCP tools into a registry.
fn register_mcp_tools(registry: ToolRegistry) -> ToolRegistry {
    let tools = crate::mcp_client::get_discovered_tools();
    let mut reg = registry;
    for tool in tools {
        let server_url = tool.server_url.clone();
        let tool_name = tool.tool_name.clone();
        reg = reg.register_fn(tool.registry_name(), move |input| {
            let url = server_url.clone();
            let name = tool_name.clone();
            async move { crate::mcp_client::call_tool(&url, &name, input).await }
        });
    }
    reg
}

/// Build the full registry (called once, result is cached).
pub(super) fn build_full_registry() -> ToolRegistry {
    let reg = ToolRegistry::new()
        .register_fn("web_search", |input| async move {
            let query = query_from_input(&input)?;
            let limit = limit_from_input(&input, 5);
            let results = crate::skills::ddg_search(&query).await?;
            Ok(json!(results.into_iter().take(limit).collect::<Vec<_>>()))
        })
        .register_ctx_fn("memory_search", |ctx, input| {
            async move {
                let query = query_from_input(&input)?;
                let limit = limit_from_input(&input, 5);
                let caller = ctx.metadata.get("caller_agent_id").cloned().unwrap_or_default();
                let results = crate::memory::memory_search(&query, limit, &caller)
                    .map_err(|e| e.to_string())?;
                Ok(json!(results))
            }
            .boxed()
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
            let all = crate::skills::list_skills();
            if let Some(query) = optional_string_field(&input, "query") {
                let recommended = crate::skills::recommended_skills(&query, &all);
                if !recommended.is_empty() {
                    return Ok(json!(recommended));
                }
            }
            Ok(json!(all))
        })
        .register_fn("skill_execute", |input| async move {
            let skill_id   = required_string_field(&input, "skill_id")?;
            let user_input = optional_string_field(&input, "user_input").unwrap_or_default();
            let agent_id   = optional_string_field(&input, "agent_id");
            let result = crate::skills::execute_skill(
                &skill_id,
                &user_input,
                agent_id.as_deref(),
            )
            .await?;
            Ok(json!({ "skill_id": skill_id, "result": result }))
        })
        .register_ctx_fn("behavior_evaluate", |ctx, input| {
            async move {
                let persona = Persona::cached().map_err(|e| e.to_string())?;
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
            let max_bytes = crate::persona::Persona::cached()
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
            if let Err(e) = crate::memory::refresh_codebase_index() {
                eprintln!("[tool] codebase index refresh failed: {e}");
            }
            Ok(json!({
                "path": safe_path.display().to_string(),
                "bytes_written": bytes,
            }))
        })
        .register_fn("file_diff", |input| async move {
            use crate::platform::NoWindow;
            let path_arg = optional_string_field(&input, "path");
            let mut cmd = std::process::Command::new("git");
            cmd.no_window().arg("diff").arg("HEAD");
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
            let mut allowed: Vec<String> = crate::persona::Persona::cached()
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

            use crate::platform::NoWindow;
            let output = std::process::Command::new(program)
                .no_window()
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
            if let Err(e) = crate::memory::refresh_codebase_index() {
                eprintln!("[tool] codebase index refresh failed: {e}");
            }
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
            use crate::platform::NoWindow;
            let out = std::process::Command::new("git")
                .no_window()
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
            use crate::platform::NoWindow;
            let limit = limit_from_input(&input, 10);
            let out = std::process::Command::new("git")
                .no_window()
                .args(["log", "--oneline", &format!("-{limit}")])
                .output()
                .map_err(|e| format!("git log failed: {e}"))?;
            let log = String::from_utf8_lossy(&out.stdout).to_string();
            Ok(json!({ "log": log }))
        })
        .register_ctx_fn("web_navigate", |ctx, input| {
            async move {
            // ── Actions ──────────────────────────────────────────────
            // Navigation:  goto, screenshot, title, url, close
            // DOM:         click, type, read, eval, wait, exists, count, attr, value
            // Input:       key, select, scroll, scroll_to
            // Coordinate:  click_point, hover, hover_point
            // Tabs:        new_tab, switch_tab, close_tab, list_tabs
            // Data:        cookies, set_cookie, delete_cookie, localStorage_get, localStorage_set
            // Network:     network, console
            // Advanced:    viewport, pdf, file_upload, iframe_eval, drag, http_auth
            let action = optional_string_field(&input, "action")
                .unwrap_or_else(|| "goto".to_string());
            let target = optional_string_field(&input, "target").unwrap_or_default();
            let text = optional_string_field(&input, "text").unwrap_or_default();

            let action_label = action.clone(); // preserved for timeout diagnostics (action moves into closure)
            let test_run_id = ctx.metadata.get("test_run_id").cloned(); // Extract test_run_id before spawn_blocking
            let blocking_fut = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
                use crate::browser;
                match action.as_str() {
                    // ── Navigation ───────────────────────────────────
                    "goto" => {
                        if target.is_empty() { return Err("'goto' requires a 'target' URL".into()); }
                        browser::navigate(&target)?;
                        let png = browser::screenshot()?;
                        crate::events::publish(crate::events::AgentEvent::BrowserScreenshotReady {
                            png_bytes: png, url: target.clone(),
                        });
                        Ok(json!({ "status": "navigated", "url": target }))
                    }
                    "screenshot" => {
                        let png = browser::screenshot()?;
                        let url = browser::current_url().unwrap_or_default();
                        crate::events::publish(crate::events::AgentEvent::BrowserScreenshotReady {
                            png_bytes: png, url: url.clone(),
                        });
                        Ok(json!({ "status": "screenshot captured", "url": url }))
                    }
                    "title" => Ok(json!({ "title": browser::page_title()? })),
                    "url"   => Ok(json!({ "url": browser::current_url()? })),
                    "close" => { browser::close(); Ok(json!({ "status": "closed" })) }

                    // ── DOM interaction ──────────────────────────────
                    "click" => {
                        if target.is_empty() { return Err("'click' requires 'target' selector".into()); }
                        browser::click(&target)?;
                        Ok(json!({ "status": "clicked", "selector": target }))
                    }
                    "type" => {
                        if target.is_empty() { return Err("'type' requires 'target' selector".into()); }
                        browser::type_text(&target, &text)?;
                        Ok(json!({ "status": "typed", "selector": target, "length": text.len() }))
                    }
                    "read" => {
                        if target.is_empty() { return Err("'read' requires 'target' selector".into()); }
                        Ok(json!({ "selector": target, "text": browser::get_text(&target)? }))
                    }
                    "eval" => {
                        if target.is_empty() { return Err("'eval' requires 'target' JS expression".into()); }
                        Ok(json!({ "result": browser::evaluate_js(&target)? }))
                    }
                    "wait" => {
                        if target.is_empty() { return Err("'wait' requires 'target' selector or ms number".into()); }
                        // Plain number → millisecond sleep (e.g. {"action":"wait","target":"2000"}).
                        if let Ok(ms) = target.trim().parse::<u64>() {
                            std::thread::sleep(std::time::Duration::from_millis(ms));
                            Ok(json!({ "status": "slept", "ms": ms }))
                        } else {
                            let ms = input.get("timeout").and_then(|v| v.as_u64()).unwrap_or(5000);
                            browser::wait_for_ms(&target, ms)?;
                            Ok(json!({ "status": "found", "selector": target }))
                        }
                    }
                    "exists" => {
                        if target.is_empty() { return Err("'exists' requires 'target' selector".into()); }
                        Ok(json!({ "selector": target, "exists": browser::element_exists(&target)? }))
                    }
                    "count" => {
                        if target.is_empty() { return Err("'count' requires 'target' selector".into()); }
                        Ok(json!({ "selector": target, "count": browser::element_count(&target)? }))
                    }
                    "attr" => {
                        if target.is_empty() { return Err("'attr' requires 'target' selector".into()); }
                        if text.is_empty() { return Err("'attr' requires 'text' = attribute name".into()); }
                        Ok(json!({ "selector": target, "attribute": &text, "value": browser::get_attribute(&target, &text)? }))
                    }
                    "value" => {
                        if target.is_empty() { return Err("'value' requires 'target' selector".into()); }
                        Ok(json!({ "selector": target, "value": browser::get_value(&target)? }))
                    }

                    // ── Keyboard / input ─────────────────────────────
                    "key" => {
                        if target.is_empty() { return Err("'key' requires 'target' key name".into()); }
                        browser::press_key(&target)?;
                        Ok(json!({ "status": "pressed", "key": target }))
                    }
                    "select" => {
                        if target.is_empty() { return Err("'select' requires 'target' selector".into()); }
                        if text.is_empty() { return Err("'select' requires 'text' = option value".into()); }
                        browser::select_option(&target, &text)?;
                        Ok(json!({ "status": "selected", "selector": target, "value": text }))
                    }
                    "scroll" => {
                        let x = input.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let y = input.get("y").and_then(|v| v.as_f64()).unwrap_or(300.0);
                        browser::scroll_by(x, y)?;
                        Ok(json!({ "status": "scrolled", "x": x, "y": y }))
                    }
                    "scroll_to" => {
                        if target.is_empty() { return Err("'scroll_to' requires 'target' selector".into()); }
                        browser::scroll_into_view(&target)?;
                        Ok(json!({ "status": "scrolled_to", "selector": target }))
                    }

                    // ── Coordinate interaction ──────────────────────
                    "click_point" => {
                        let x = input.get("x").and_then(|v| v.as_f64()).ok_or("'click_point' requires 'x'")?;
                        let y = input.get("y").and_then(|v| v.as_f64()).ok_or("'click_point' requires 'y'")?;
                        browser::click_point(x, y)?;
                        Ok(json!({ "status": "clicked", "x": x, "y": y }))
                    }
                    "hover" => {
                        if target.is_empty() { return Err("'hover' requires 'target' selector".into()); }
                        browser::hover(&target)?;
                        Ok(json!({ "status": "hovered", "selector": target }))
                    }
                    "hover_point" => {
                        let x = input.get("x").and_then(|v| v.as_f64()).ok_or("'hover_point' requires 'x'")?;
                        let y = input.get("y").and_then(|v| v.as_f64()).ok_or("'hover_point' requires 'y'")?;
                        browser::hover_point(x, y)?;
                        Ok(json!({ "status": "hovered", "x": x, "y": y }))
                    }

                    // ── Tabs ─────────────────────────────────────────
                    "new_tab" => {
                        let idx = browser::new_tab()?;
                        if !target.is_empty() { browser::navigate(&target)?; }
                        Ok(json!({ "tab_index": idx }))
                    }
                    "switch_tab" => {
                        let idx = input.get("index").and_then(|v| v.as_u64())
                            .ok_or("'switch_tab' requires 'index'")? as usize;
                        browser::switch_tab(idx)?;
                        Ok(json!({ "status": "switched", "tab_index": idx }))
                    }
                    "close_tab" => {
                        let idx = input.get("index").and_then(|v| v.as_u64())
                            .ok_or("'close_tab' requires 'index'")? as usize;
                        browser::close_tab(idx)?;
                        Ok(json!({ "status": "tab_closed", "index": idx }))
                    }
                    "list_tabs" => {
                        let tabs = browser::list_tabs()?;
                        let active = browser::active_tab()?;
                        let arr: Vec<serde_json::Value> = tabs.into_iter()
                            .map(|(i, u)| json!({"index": i, "url": u, "active": i == active}))
                            .collect();
                        Ok(json!({ "tabs": arr }))
                    }

                    // ── Cookies ──────────────────────────────────────
                    "cookies" => {
                        let raw = browser::get_cookies()?;
                        let val: serde_json::Value = serde_json::from_str(&raw).unwrap_or(json!([]));
                        Ok(json!({ "cookies": val }))
                    }
                    "set_cookie" => {
                        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                        let value = input.get("value").and_then(|v| v.as_str()).unwrap_or_default();
                        let domain = input.get("domain").and_then(|v| v.as_str()).unwrap_or_default();
                        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("/");
                        browser::set_cookie(name, value, domain, path)?;
                        Ok(json!({ "status": "cookie_set", "name": name }))
                    }
                    "delete_cookie" => {
                        if target.is_empty() { return Err("'delete_cookie' requires 'target' cookie name".into()); }
                        browser::delete_cookie(&target)?;
                        Ok(json!({ "status": "cookie_deleted", "name": target }))
                    }

                    // ── Storage ──────────────────────────────────────
                    "localStorage_get" => {
                        if target.is_empty() { return Err("requires 'target' key".into()); }
                        Ok(json!({ "key": target, "value": browser::local_storage_get(&target)? }))
                    }
                    "localStorage_set" => {
                        if target.is_empty() { return Err("requires 'target' key".into()); }
                        browser::local_storage_set(&target, &text)?;
                        Ok(json!({ "status": "set", "key": target }))
                    }

                    // ── Network / Console ────────────────────────────
                    "network" => {
                        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                        let raw = browser::captured_requests(limit)?;
                        let val: serde_json::Value = serde_json::from_str(&raw).unwrap_or(json!([]));
                        Ok(json!({ "requests": val }))
                    }
                    "console" => {
                        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                        let raw = browser::console_messages(limit)?;
                        let val: serde_json::Value = serde_json::from_str(&raw).unwrap_or(json!([]));
                        Ok(json!({ "messages": val }))
                    }
                    "install_capture" => {
                        browser::install_console_capture()?;
                        browser::install_network_capture()?;
                        Ok(json!({ "status": "console+network capture installed" }))
                    }

                    // ── Advanced ─────────────────────────────────────
                    "viewport" => {
                        let w = input.get("width").and_then(|v| v.as_u64()).unwrap_or(1280) as u32;
                        let h = input.get("height").and_then(|v| v.as_u64()).unwrap_or(800) as u32;
                        let scale = input.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
                        let mobile = input.get("mobile").and_then(|v| v.as_bool()).unwrap_or(false);
                        browser::set_viewport(w, h, scale, mobile)?;
                        Ok(json!({ "status": "viewport_set", "width": w, "height": h }))
                    }
                    "pdf" => {
                        let bytes = browser::pdf()?;
                        Ok(json!({ "status": "pdf_exported", "bytes": bytes.len() }))
                    }
                    "drag" => {
                        let fx = input.get("from_x").and_then(|v| v.as_f64()).ok_or("requires 'from_x'")?;
                        let fy = input.get("from_y").and_then(|v| v.as_f64()).ok_or("requires 'from_y'")?;
                        let tx = input.get("to_x").and_then(|v| v.as_f64()).ok_or("requires 'to_x'")?;
                        let ty = input.get("to_y").and_then(|v| v.as_f64()).ok_or("requires 'to_y'")?;
                        browser::drag(fx, fy, tx, ty)?;
                        Ok(json!({ "status": "dragged" }))
                    }
                    "http_auth" => {
                        let user = input.get("username").and_then(|v| v.as_str()).unwrap_or_default();
                        let pass = input.get("password").and_then(|v| v.as_str()).unwrap_or_default();
                        browser::set_http_auth(user, pass)?;
                        Ok(json!({ "status": "auth_set" }))
                    }

                    // ── Accessibility tree (literal text, fast, Flutter-OK) ─
                    "enable_a11y" => {
                        crate::browser_ax::enable_flutter_semantics()?;
                        // Poll until flt-semantics-host is non-empty (Flutter fills it async).
                        // Max 15 × 200ms = 3 s; if still empty after that, return what we have.
                        let mut shadow_ready = false;
                        for _ in 0..15 {
                            let count = browser::evaluate_js(
                                "document.querySelector('flt-semantics-host')?.childElementCount||0"
                            ).unwrap_or_default();
                            if count.trim() != "0" {
                                shadow_ready = true;
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        let ax_count = {
                            match crate::browser_ax::get_full_tree(false) {
                                Ok(nodes) => nodes.len(),
                                Err(_) => 0,
                            }
                        };
                        Ok(json!({ "status": "semantics enabled", "ax_node_count": ax_count, "shadow_ready": shadow_ready }))
                    }
                    "wait_for_ax_ready" => {
                        let min_nodes = input.get("min_nodes").and_then(Value::as_u64).unwrap_or(20) as usize;
                        let timeout_ms = input.get("timeout_ms").or_else(|| input.get("timeout")).and_then(Value::as_u64).unwrap_or(10000);
                        let (elapsed, count) = crate::browser_ax::wait_for_ax_ready(min_nodes, timeout_ms)?;
                        Ok(json!({ "elapsed_ms": elapsed, "node_count": count }))
                    }

                    // ── Flutter Shadow DOM (bypasses CDP AX protocol) ─────────
                    "shadow_find" => {
                        let role = optional_string_field(&input, "role");
                        let name = optional_string_field(&input, "name_regex")
                            .or_else(|| optional_string_field(&input, "name"));
                        let (x, y, label) = browser::shadow_find(role.as_deref(), name.as_deref())?;
                        Ok(json!({ "found": true, "x": x, "y": y, "label": label }))
                    }
                    "shadow_click" => {
                        let role = optional_string_field(&input, "role");
                        let name = optional_string_field(&input, "name_regex")
                            .or_else(|| optional_string_field(&input, "name"));
                        let label = browser::shadow_click(role.as_deref(), name.as_deref())?;
                        Ok(json!({ "status": "clicked", "label": label }))
                    }
                    "shadow_type" => {
                        let role = optional_string_field(&input, "role");
                        let name = optional_string_field(&input, "name_regex")
                            .or_else(|| optional_string_field(&input, "name"));
                        let text_val = input.get("text").and_then(Value::as_str)
                            .ok_or("'shadow_type' requires 'text'")?;
                        browser::shadow_type(role.as_deref(), name.as_deref(), text_val)?;
                        Ok(json!({ "status": "typed", "text": text_val }))
                    }
                    // Flutter-native typing: shadow_click → wait 300ms → flutter_type
                    // Use this instead of shadow_type for Flutter textboxes (which need
                    // keydown events, not Input.InsertText).
                    "flutter_type" => {
                        // Accept both string "50" and number 50
                        let text_owned = input.get("text")
                            .map(|v| if let Some(s) = v.as_str() { s.to_string() } else { v.to_string().trim_matches('"').to_string() })
                            .ok_or("'flutter_type' requires 'text'")?;
                        browser::flutter_type(&text_owned)?;
                        Ok(json!({ "status": "typed", "text": text_owned }))
                    }
                    "flutter_enter" => {
                        // Send Enter key to the active flt-text-editing input — submits chat/form
                        let result = browser::flutter_enter()?;
                        Ok(json!({ "status": "ok", "result": result }))
                    }
                    "shadow_type_flutter" => {
                        // All-in-one: find + click + wait 350ms + flutter_type
                        let role = optional_string_field(&input, "role");
                        let name = optional_string_field(&input, "name_regex")
                            .or_else(|| optional_string_field(&input, "name"));
                        let text_owned = input.get("text")
                            .map(|v| if let Some(s) = v.as_str() { s.to_string() } else { v.to_string().trim_matches('"').to_string() })
                            .ok_or("'shadow_type_flutter' requires 'text'")?;
                        let label = browser::shadow_click(role.as_deref(), name.as_deref())?;
                        std::thread::sleep(std::time::Duration::from_millis(350));
                        browser::flutter_type(&text_owned)?;
                        Ok(json!({ "status": "typed", "label": label, "text": text_owned }))
                    }
                    "shadow_dump" => {
                        let items = browser::shadow_dump()?;
                        Ok(json!({ "count": items.len(), "elements": items }))
                    }

                    "ax_tree" => {
                        let include_ignored = input.get("include_ignored").and_then(Value::as_bool).unwrap_or(false);
                        let nodes = crate::browser_ax::get_full_tree(include_ignored)?;
                        
                        // P1.2 optimization: Use A11y tree auto-diffing to reduce tokens
                        if let Some(rid) = &test_run_id {
                            // Serialize nodes to tree format for diffing
                            let tree_value: Value = serde_json::to_value(&nodes)
                                .unwrap_or(json!({}));
                            
                            // Determine if this is the first call
                            let mut is_first = false;
                            crate::test_runner::runs::mutate_ax_diff_context(rid, |diff_ctx| {
                                is_first = diff_ctx.set_baseline_if_first(tree_value.clone());
                            });
                            
                            // If not first call, compute and return diff
                            if !is_first {
                                if let Some(diff_ctx) = crate::test_runner::runs::get_ax_diff_context(rid) {
                                    let diff_result = diff_ctx.compute_diff(&tree_value);
                                    return Ok(json!({
                                        "count": nodes.len(),
                                        "nodes": nodes,
                                        "diff_mode": true,
                                        "diff_summary": diff_result,
                                    }));
                                }
                            }
                            
                            // First call or no diff context: return full tree
                            Ok(json!({
                                "count": nodes.len(),
                                "nodes": nodes,
                                "first_ax_tree_call": is_first,
                            }))
                        } else {
                            // No test_run_id in context: return full tree
                            Ok(json!({ "count": nodes.len(), "nodes": nodes }))
                        }
                    }
                    "ax_find" => {
                        let role = optional_string_field(&input, "role");
                        let name = optional_string_field(&input, "name");
                        if role.is_none() && name.is_none() {
                            return Err("'ax_find' requires 'role' and/or 'name'".into());
                        }
                        let name_regex = optional_string_field(&input, "name_regex");
                        let not_name_matches: Vec<String> = input
                            .get("not_name_matches")
                            .and_then(Value::as_array)
                            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();
                        let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(1) as usize;

                        if limit <= 1 {
                            match crate::browser_ax::find_by_role_and_name(
                                role.as_deref(), name.as_deref(),
                                name_regex.as_deref(), &not_name_matches,
                            )? {
                                Some(n) => Ok(json!({ "found": true, "node": n })),
                                None    => Ok(json!({ "found": false, "node": null })),
                            }
                        } else {
                            let nodes = crate::browser_ax::find_all_by_role_and_name(
                                role.as_deref(), name.as_deref(),
                                name_regex.as_deref(), &not_name_matches, limit,
                            )?;
                            Ok(json!({
                                "found": !nodes.is_empty(),
                                "count": nodes.len(),
                                "nodes": nodes,
                            }))
                        }
                    }
                    "ax_snapshot" => {
                        let snap_id_arg = optional_string_field(&input, "id");
                        let id = crate::browser_ax::ax_snapshot(snap_id_arg.as_deref())?;
                        Ok(json!({ "snapshot_id": id }))
                    }
                    "ax_diff" => {
                        let before = input.get("before_id").and_then(Value::as_str)
                            .ok_or("'ax_diff' requires 'before_id'")?;
                        let after = input.get("after_id").and_then(Value::as_str)
                            .ok_or("'ax_diff' requires 'after_id'")?;
                        let diff = crate::browser_ax::ax_diff(before, after)?;
                        Ok(json!({
                            "added_count":   diff.added.len(),
                            "removed_count": diff.removed.len(),
                            "changed_count": diff.changed.len(),
                            "added":   diff.added.iter().map(|n| json!({"node_id": n.node_id, "role": n.role, "name": n.name})).collect::<Vec<_>>(),
                            "removed": diff.removed.iter().map(|n| json!({"node_id": n.node_id, "role": n.role, "name": n.name})).collect::<Vec<_>>(),
                            "changed": diff.changed,
                        }))
                    }
                    "wait_for_ax_change" => {
                        let baseline_id = input.get("baseline_id").and_then(Value::as_str)
                            .ok_or("'wait_for_ax_change' requires 'baseline_id'")?;
                        let timeout_ms = input.get("timeout").and_then(Value::as_u64).unwrap_or(5000);
                        let (new_id, diff) = crate::browser_ax::wait_for_ax_change(baseline_id, timeout_ms)?;
                        Ok(json!({
                            "new_snapshot_id": new_id,
                            "added_count":   diff.added.len(),
                            "removed_count": diff.removed.len(),
                            "changed_count": diff.changed.len(),
                        }))
                    }
                    "ax_value" => {
                        let backend_id = input.get("backend_id").and_then(Value::as_u64)
                            .ok_or("'ax_value' requires 'backend_id' (number)")?;
                        let text = crate::browser_ax::read_node_text(backend_id as u32)?;
                        Ok(json!({ "backend_id": backend_id, "text": text }))
                    }
                    "ax_click" => {
                        let backend_id = input.get("backend_id").and_then(Value::as_u64)
                            .ok_or("'ax_click' requires 'backend_id' (number)")?;
                        crate::browser_ax::click_backend(backend_id as u32)?;
                        Ok(json!({ "status": "clicked", "backend_id": backend_id }))
                    }
                    "ax_focus" => {
                        let backend_id = input.get("backend_id").and_then(Value::as_u64)
                            .ok_or("'ax_focus' requires 'backend_id' (number)")?;
                        crate::browser_ax::focus_backend(backend_id as u32)?;
                        Ok(json!({ "status": "focused", "backend_id": backend_id }))
                    }
                    "ax_type" => {
                        let backend_id = input.get("backend_id").and_then(Value::as_u64)
                            .ok_or("'ax_type' requires 'backend_id' (number)")?;
                        crate::browser_ax::type_into_backend(backend_id as u32, &text)?;
                        Ok(json!({ "status": "typed", "backend_id": backend_id, "length": text.len() }))
                    }
                    "ax_type_verified" => {
                        let backend_id = input.get("backend_id").and_then(Value::as_u64)
                            .ok_or("'ax_type_verified' requires 'backend_id' (number)")?;
                        let r = crate::browser_ax::type_into_backend_verified(backend_id as u32, &text)?;
                        Ok(json!(r))
                    }

                    // ── Test isolation ───────────────────────────
                    "clear_state" => {
                        browser::clear_browser_state()?;
                        Ok(json!({ "status": "cleared" }))
                    }

                    "set_viewport" => {
                        let w = input.get("width").and_then(Value::as_u64).unwrap_or(1440) as u32;
                        let h = input.get("height").and_then(Value::as_u64).unwrap_or(1600) as u32;
                        let scale = input.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
                        let mobile = input.get("mobile").and_then(Value::as_bool).unwrap_or(false);
                        browser::set_viewport(w, h, scale, mobile)?;
                        Ok(json!({ "status": "viewport_set", "width": w, "height": h }))
                    }

                    // ── Multi-tab / popup ────────────────────────
                    "wait_new_tab" => {
                        let timeout = input.get("timeout").and_then(Value::as_u64).unwrap_or(10000);
                        // baseline=None → fn measures from same source as its loop
                        let idx = browser::wait_for_new_tab(None, timeout)?;
                        Ok(json!({ "status": "new tab opened", "active_tab": idx }))
                    }

                    // ── Network ──────────────────────────────────
                    "wait_request" => {
                        if target.is_empty() { return Err("'wait_request' requires 'target' = URL substring".into()); }
                        let timeout = input.get("timeout").and_then(Value::as_u64).unwrap_or(10000);
                        let raw = browser::wait_for_request(&target, timeout)?;
                        let val: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
                        Ok(json!({ "request": val }))
                    }

                    // ── Vision: screenshot + LLM analysis ────────
                    "screenshot_analyze" => {
                        // Take screenshot, send to vision LLM with prompt, return analysis
                        if target.is_empty() {
                            return Err("'screenshot_analyze' requires 'target' = analysis prompt".into());
                        }
                        // Capture screenshot (blocking)
                        let png = browser::screenshot()?;
                        // Return the png + prompt for async vision call below
                        Ok(json!({ "__vision": true, "prompt": target, "png_len": png.len() }))
                    }

                    other => Err(format!("Unknown web_navigate action: {other}")),
                }
            });
            // ── CDP call timeout ──────────────────────────────────────────────
            // If Chrome crashes mid-call the spawned blocking thread can block
            // indefinitely on a dead WebSocket.  120 s covers the slowest
            // legitimate navigations; on expiry we reset the singleton so the
            // next call triggers a fresh Chrome launch instead of reusing the
            // dead process.
            let result = match tokio::time::timeout(
                std::time::Duration::from_secs(120),
                blocking_fut,
            ).await {
                Ok(join_result) => join_result.map_err(|e| format!("spawn_blocking: {e}"))??,
                Err(_elapsed) => {
                    tracing::warn!(
                        "[browser] web_navigate '{}' timed out (120 s) — closing browser singleton",
                        action_label
                    );
                    crate::browser::close();
                    return Err(format!(
                        "CDP call '{action_label}' timed out (120 s) — \
                         browser singleton reset; Chrome will re-launch on next call"
                    ));
                }
            };

            // Handle vision analysis (requires async LLM call)
            if result.get("__vision").and_then(|v| v.as_bool()).unwrap_or(false) {
                let mut prompt = result["prompt"].as_str().unwrap_or("Describe this page").to_string();
                let screenshot_b64 = result
                    .get("screenshot_b64")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                
                // P1.1 optimization: Prepare SoM (Set-of-Mark) visual labels
                // if AX tree is available (interactive elements → numbered labels)
                let run_id = ctx.metadata.get("test_run_id");
                if !screenshot_b64.is_empty() && run_id.is_some() {
                    // Try to fetch recent AX tree (if available from last ax_tree call)
                    // For MVP, SoM preparation is optional; if unavailable, continue with plain vision
                    let run_id_str = run_id.unwrap();
                    if let Some(recent_ax_nodes) = crate::test_runner::runs::get_recent_ax_nodes(run_id_str) {
                        let som_renderer = crate::test_runner::som_renderer::SoMRenderer::with_defaults();
                        
                        // Prepare label map (label_id → x,y coordinates)
                        match som_renderer.prepare_label_map(&recent_ax_nodes) {
                            Ok(label_map) if !label_map.is_empty() => {
                                // Store label map for potential execution phase (e.g., "click label 5")
                                crate::test_runner::runs::set_som_label_map(run_id_str, label_map.clone());
                                
                                // Render labels on screenshot (MVP: no-op, but API ready)
                                match som_renderer.render_labels(&screenshot_b64, &label_map) {
                                    Ok(marked_img) => {
                                        // MVP: render_labels currently returns input unchanged
                                        // Once image crate is integrated, this will have actual label drawings
                                        let _ = marked_img; // TODO: integrate with marked_img when rendering complete
                                        prompt = format!(
                                            "{}\n\n【重要】圖片上已標記可互動元件的數字編號 (1, 2, 3, ...)。\n\
                                            若需點擊某個元件，直接說『點擊 5 號』而不是猜測座標。",
                                            prompt
                                        );
                                        tracing::debug!("[vision] SoM labels applied");
                                    }
                                    Err(e) => {
                                        tracing::warn!("[vision] SoM rendering failed: {}, using plain vision", e);
                                    }
                                }
                            }
                            _ => {
                                tracing::debug!("[vision] SoM label map empty or unavailable");
                            }
                        }
                    }
                }
                
                // Preserve size_bytes + url from the blocking screenshot call so that
                // executor.rs::is_all_black_screenshot() can detect black frames even
                // when the action is screenshot_analyze (not just screenshot).
                let size_bytes = result.get("png_len").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
                let url = tokio::task::spawn_blocking(|| {
                    crate::browser::get_current_url().unwrap_or_default()
                }).await.unwrap_or_default();
                
                // P1.1 optimization: Check screenshot cache before calling vision LLM
                let mut analysis = None;
                let mut cache_hit = false;
                
                if let Some(run_id_str) = run_id {
                    // Get PNG bytes to compute hash for cache lookup
                    if let Ok(png) = tokio::task::spawn_blocking(crate::browser::screenshot).await {
                        if let Ok(png_bytes) = png {
                            // Compute SHA256 hash of PNG
                            use sha2::{Sha256, Digest};
                            let mut hasher = Sha256::new();
                            hasher.update(&png_bytes);
                            let hash_hex = format!("{:x}", hasher.finalize());
                            
                            // Check cache
                            if let Some(cached) = crate::test_runner::runs::get_screenshot_cache(run_id_str, &hash_hex) {
                                analysis = Some(cached);
                                cache_hit = true;
                                tracing::debug!("[vision] cache HIT for {} bytes", png_bytes.len());
                            }
                        }
                    }
                }
                
                // If cache miss, call vision LLM
                if analysis.is_none() {
                    let llm = crate::llm::shared_llm();
                    let client = crate::llm::shared_http();
                    match crate::llm::analyze_screenshot(&client, &llm, &prompt).await {
                        Ok(result) => {
                            analysis = Some(result.clone());
                            // Store in cache for future use
                            if let Some(run_id_str) = run_id {
                                if let Ok(png) = tokio::task::spawn_blocking(crate::browser::screenshot).await {
                                    if let Ok(png_bytes) = png {
                                        use sha2::{Sha256, Digest};
                                        let mut hasher = Sha256::new();
                                        hasher.update(&png_bytes);
                                        let hash_hex = format!("{:x}", hasher.finalize());
                                        crate::test_runner::runs::set_screenshot_cache(run_id_str, hash_hex, result);
                                    }
                                }
                            }
                            tracing::debug!("[vision] cache MISS, called LLM");
                        }
                        Err(e) => {
                            return Err(format!("vision analysis failed: {e}"));
                        }
                    }
                }
                
                return Ok(json!({
                    "analysis": analysis.unwrap_or_default(),
                    "size_bytes": size_bytes,
                    "url": url,
                    "cache_hit": cache_hit,
                }));
            }

            Ok(result)
            }.boxed()
        })
        .register_ctx_fn("expand_observation", |ctx, input| {
            async move {
                // Retrieve the full (un-truncated) tool observation for a given
                // step of the CURRENT test run.  run_id comes from context
                // metadata set by run_test_with_run_id() — if unset, this tool
                // can't help (e.g. called outside a test).
                let step = input.get("step").and_then(Value::as_u64)
                    .ok_or_else(|| "'expand_observation' requires 'step' (0-indexed number)".to_string())? as usize;

                let run_id = ctx.metadata.get("test_run_id").cloned()
                    .ok_or_else(|| "expand_observation can only be called during a test run (no test_run_id in context)".to_string())?;

                match crate::test_runner::runs::get_full_observation(&run_id, step) {
                    Some(content) => Ok(json!({
                        "run_id": run_id,
                        "step": step,
                        "content": content,
                        "char_count": content.chars().count(),
                    })),
                    None => Err(format!(
                        "no observation at step {step} for run {run_id} (valid range: 0..N where N is current step count)"
                    )),
                }
            }.boxed()
        })
        .register_ctx_fn("run_test", |ctx, input| {
            async move {
                let test_id = required_string_field(&input, "test_id")?;
                let auto_fix = input.get("auto_fix").and_then(Value::as_bool).unwrap_or(false);
                let tag = optional_string_field(&input, "tag");

                if test_id == "*" {
                    let results = crate::test_runner::run_all(ctx, tag.as_deref(), auto_fix).await;
                    let summary: Vec<serde_json::Value> = results.iter().map(|r| json!({
                        "test_id": r.test_id,
                        "status": format!("{:?}", r.status).to_lowercase(),
                        "iterations": r.iterations,
                        "duration_ms": r.duration_ms,
                        "error": r.error_message,
                    })).collect();
                    Ok(json!({
                        "count": results.len(),
                        "passed": results.iter().filter(|r| matches!(r.status, crate::test_runner::TestStatus::Passed)).count(),
                        "results": summary,
                    }))
                } else {
                    let result = crate::test_runner::run_test(ctx, &test_id, auto_fix).await?;
                    Ok(json!({
                        "test_id": result.test_id,
                        "status": format!("{:?}", result.status).to_lowercase(),
                        "iterations": result.iterations,
                        "duration_ms": result.duration_ms,
                        "error": result.error_message,
                        "analysis": result.final_analysis,
                        "screenshot": result.screenshot_path,
                        "screenshot_error": result.screenshot_error,
                        "steps": result.history.len(),
                    }))
                }
            }.boxed()
        })
        .register_fn("list_tests", |_input| async move {
            let tests = crate::test_runner::list_tests();
            let items: Vec<serde_json::Value> = tests.iter().map(|t| json!({
                "id": t.id,
                "name": t.name,
                "url": t.url,
                "tags": t.tags,
            })).collect();
            Ok(json!({ "count": items.len(), "tests": items }))
        })
        .register_fn("claude_session", |input| async move {
            // Spawn a Claude Code CLI session to fix bugs in another repo.
            // repo:   "backend" | "frontend" | "sirin" | absolute path
            // prompt: full instruction to Claude
            // Optional context fields for bug reports:
            //   bug, url, error, network_log, screenshot_path
            let repo = required_string_field(&input, "repo")?;
            let prompt = optional_string_field(&input, "prompt");
            let bug = optional_string_field(&input, "bug");

            // Resolve repo path
            let cwd = if std::path::Path::new(&repo).is_absolute() {
                repo.clone()
            } else {
                crate::claude_session::repo_path(&repo)
                    .ok_or_else(|| format!("Unknown repo alias: {repo}. Use: backend, frontend, sirin, or absolute path"))?
            };

            // Build prompt from either direct prompt or bug fields
            let final_prompt = if let Some(p) = prompt {
                p
            } else if let Some(b) = bug {
                crate::claude_session::build_bug_prompt(
                    &b,
                    optional_string_field(&input, "url").as_deref(),
                    optional_string_field(&input, "error").as_deref(),
                    optional_string_field(&input, "network_log").as_deref(),
                    optional_string_field(&input, "screenshot_path").as_deref(),
                )
            } else {
                return Err("'claude_session' requires 'prompt' or 'bug' field".into());
            };

            // Run in background thread (can take minutes)
            let result = tokio::task::spawn_blocking(move || {
                crate::claude_session::run_sync(&cwd, &final_prompt)
            })
            .await
            .map_err(|e| format!("spawn_blocking: {e}"))??;

            Ok(json!({
                "success": result.success,
                "exit_code": result.exit_code,
                "output": result.output.chars().take(3000).collect::<String>(),
            }))
        })
        .register_ctx_fn("confidential_handoff", |ctx, input| {
            async move {
                let from_agent = ctx.metadata
                    .get("caller_agent_id").cloned().unwrap_or_default();
                let to_agent   = required_string_field(&input, "to_agent")?;
                let payload    = required_string_field(&input, "payload")?;
                let source_hint = optional_string_field(&input, "source_hint")
                    .unwrap_or_else(|| "agent_handoff".to_string());

                // Verify from_agent is in the recipient's trusted_senders list.
                let agents_file = crate::agent_config::AgentsFile::load()
                    .map_err(|e| e.to_string())?;
                let recipient = agents_file.agents.iter()
                    .find(|a| a.id == to_agent)
                    .ok_or_else(|| format!("Unknown recipient agent: {to_agent}"))?;
                if !recipient.memory_policy.trusted_senders.is_empty()
                    && !recipient.memory_policy.trusted_senders.contains(&from_agent)
                {
                    // Fallback: check runtime meeting-room auth.
                    if !crate::meeting::check_meeting_auth(&from_agent, &to_agent) {
                        return Err(format!(
                            "Agent '{from_agent}' is not trusted by '{to_agent}'"
                        ));
                    }
                }

                // Persist confidential memory in the recipient's namespace.
                crate::memory::memory_store(&payload, &source_hint, &to_agent, "confidential")
                    .map_err(|e| e.to_string())?;

                Ok(json!({ "status": "delivered", "recipient": to_agent }))
            }
            .boxed()
        });

    // Register external MCP tools discovered at startup.
    register_mcp_tools(reg)
}
