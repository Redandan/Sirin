//! Built-in tool registrations.
//!
//! `build_full_registry()` wires up every tool Sirin ships with:
//! search (web/memory/codebase/symbol), filesystem (read/write/patch/list),
//! task tracking, git status/log, shell exec (allowlisted), skills, behaviour
//! evaluation, call graph, web navigation, agent handoff, and plan execution.
//! External MCP tools discovered at startup are appended last.

use futures::FutureExt;
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
    input
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
        .register_fn("web_navigate", |input| async move {
            // action: "goto" | "click" | "type" | "screenshot"
            // target: URL (for goto/screenshot) or CSS selector (for click/type)
            // text:   text to type (only for "type" action)
            let action = optional_string_field(&input, "action")
                .unwrap_or_else(|| "goto".to_string());
            let target = required_string_field(&input, "target")?;
            let _text = optional_string_field(&input, "text").unwrap_or_default();

            let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
                use crate::browser::BrowserSession;
                match action.as_str() {
                    "screenshot" | "goto" => {
                        let png = BrowserSession::navigate_and_screenshot(&target)
                            .map_err(|e| e.to_string())?;
                        // Publish event so the UI can display the screenshot.
                        crate::events::publish(crate::events::AgentEvent::BrowserScreenshotReady {
                            png_bytes: png,
                            url: target.clone(),
                        });
                        Ok(json!({ "status": "screenshot captured", "url": target }))
                    }
                    "click" => {
                        // Stateless click: launch, navigate (no URL given → error),
                        // then click. Callers should prefer a goto first then click.
                        Err("'click' requires an active session; use 'goto' first".to_string())
                    }
                    "type" => {
                        Err("'type' requires an active session; use 'goto' first".to_string())
                    }
                    other => Err(format!("Unknown web_navigate action: {other}")),
                }
            })
            .await
            .map_err(|e| format!("spawn_blocking: {e}"))??;

            Ok(result)
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
