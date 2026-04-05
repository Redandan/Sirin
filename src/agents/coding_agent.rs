//! Local AI Coding Agent — executes a ReAct (Reason + Act) loop to read,
//! modify, and verify code using the tool registry.
//!
//! ## Workflow
//! 1. **Understand** the codebase with `project_overview` + `codebase_search`.
//! 2. **Plan** — single LLM call that produces a numbered step list.
//! 3. **ReAct loop** (max `max_iterations` rounds):
//!    - Prompt the LLM with the task, history, and available tools.
//!    - Parse a JSON response: `{ "thought", "action", "action_input" }`.
//!    - Execute the tool and append the observation.
//!    - If `action == "DONE"`, break and surface the `final_answer`.
//! 4. **Verify** with `cargo check` (if allowed) and collect `git diff`.
//! 5. Return a [`CodingAgentResponse`].

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::llm::call_coding_prompt;
use crate::persona::{CodingAgentConfig, Persona, TaskTracker};
use crate::sirin_log;

// ── Public request / response types ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingRequest {
    /// Natural-language description of the coding task.
    pub task: String,
    /// Override `max_iterations` from persona config.
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// When true, `file_write` calls use `dry_run = true` so nothing is written
    /// to disk.  The agent still produces the intended diff as output.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodingAgentResponse {
    /// Human-readable summary of what was accomplished.
    pub outcome: String,
    /// Files that were (or would have been, in dry-run mode) written.
    pub files_modified: Vec<String>,
    /// Number of ReAct iterations consumed.
    pub iterations_used: usize,
    /// Output of `git diff HEAD` after the task (empty if nothing changed).
    #[serde(default)]
    pub diff: Option<String>,
    /// Whether `cargo check` passed after the changes.
    #[serde(default)]
    pub verified: bool,
    /// Raw output of the verification command.
    #[serde(default)]
    pub verification_output: Option<String>,
    /// Step-by-step execution trace (thought → tool → observation).
    #[serde(default)]
    pub trace: Vec<String>,
    /// True when the agent ran in dry-run mode.
    #[serde(default)]
    pub dry_run: bool,
}

// ── Agent struct ──────────────────────────────────────────────────────────────

pub struct CodingAgent;

impl Agent for CodingAgent {
    fn name(&self) -> &'static str {
        "coding_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: CodingRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid coding request payload: {e}"))?;

            let config = Persona::load()
                .map(|p| p.coding_agent)
                .unwrap_or_default();

            if !config.enabled {
                return Err("Coding agent is disabled in persona config.".to_string());
            }

            let response = run_react_loop(ctx, &request, &config).await?;
            serde_json::to_value(response).map_err(|e| e.to_string())
        }
        .boxed()
    }
}

// ── ReAct loop ────────────────────────────────────────────────────────────────

async fn run_react_loop(
    ctx: &AgentContext,
    request: &CodingRequest,
    config: &CodingAgentConfig,
) -> Result<CodingAgentResponse, String> {
    let max_iter = request.max_iterations.unwrap_or(config.max_iterations);
    let dry_run = request.dry_run || !config.auto_approve_writes;

    sirin_log!("[coding_agent] task='{}' max_iter={max_iter} dry_run={dry_run}", request.task);
    ctx.record_system_event(
        "adk_coding_agent_start",
        Some(preview_text(&request.task)),
        Some("RUNNING"),
        Some(format!("max_iter={max_iter} dry_run={dry_run}")),
    );

    // ── Step 1: gather project context ────────────────────────────────────────
    let project_ctx = gather_project_context(ctx, &request.task).await;

    // ── Step 2: planning call ─────────────────────────────────────────────────
    let plan = make_plan(ctx, &request.task, &project_ctx).await;
    sirin_log!("[coding_agent] plan ready ({} chars)", plan.len());

    // ── Step 3: ReAct loop ────────────────────────────────────────────────────
    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut files_modified: Vec<String> = Vec::new();
    let mut final_answer = String::new();
    let tool_list = describe_tools();

    for iteration in 0..max_iter {
        let prompt = build_react_prompt(
            &request.task,
            &project_ctx,
            &plan,
            &history,
            &tool_list,
            dry_run,
        );

        let raw = call_coding_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt)
            .await
            .map_err(|e| format!("LLM error on iteration {iteration}: {e}"))?;

        let step = parse_react_step(&raw);
        sirin_log!(
            "[coding_agent] iter={} action={} thought={}",
            iteration,
            step.action,
            preview_text(&step.thought)
        );

        ctx.record_system_event(
            format!("adk_coding_iter_{iteration}"),
            Some(format!("action={}", step.action)),
            Some("RUNNING"),
            Some(preview_text(&step.thought)),
        );

        if step.action == "DONE" {
            final_answer = step.final_answer.unwrap_or_else(|| step.thought.clone());
            history.push(HistoryEntry {
                thought: step.thought,
                action: "DONE".to_string(),
                action_input: json!({}),
                observation: "Task complete.".to_string(),
            });
            break;
        }

        // Execute the tool.
        let tool_input = if dry_run && step.action == "file_write" {
            let mut input = step.action_input.clone();
            if let Some(obj) = input.as_object_mut() {
                obj.insert("dry_run".to_string(), json!(true));
            }
            input
        } else {
            step.action_input.clone()
        };

        let observation = match ctx.call_tool(&step.action, tool_input).await {
            Ok(v) => {
                // Track which files were written.
                if step.action == "file_write" || step.action == "file_patch" {
                    if let Some(path) = v.get("path").and_then(Value::as_str) {
                        if !files_modified.contains(&path.to_string()) {
                            files_modified.push(path.to_string());
                        }
                    }
                }
                // Track files touched via plan_execute steps.
                if step.action == "plan_execute" {
                    if let Some(results) = v.get("results").and_then(Value::as_array) {
                        for r in results {
                            if let Some(result) = r.get("result") {
                                if let Some(path) = result.get("path").and_then(Value::as_str) {
                                    if !files_modified.contains(&path.to_string()) {
                                        files_modified.push(path.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                format_tool_output(&v)
            }
            Err(e) => format!("ERROR: {e}"),
        };

        history.push(HistoryEntry {
            thought: step.thought,
            action: step.action,
            action_input: step.action_input,
            observation: observation.clone(),
        });

        // Bail early if we hit a repeated error pattern.
        if observation.starts_with("ERROR:") {
            sirin_log!("[coding_agent] tool error: {observation}");
        }
    }

    // ── Step 4: verification ──────────────────────────────────────────────────
    let (verified, verification_output) = if !dry_run && config.allowed_commands
        .iter()
        .any(|cmd| cmd == "cargo check")
    {
        verify_build(ctx).await
    } else {
        (false, None)
    };

    // ── Step 5: diff ──────────────────────────────────────────────────────────
    let diff = if !dry_run {
        get_diff(ctx).await
    } else {
        None
    };

    let iterations_used = history.iter().filter(|h| h.action != "DONE").count();
    let trace: Vec<String> = history
        .iter()
        .map(|h| {
            format!(
                "💭 {}\n🔧 {}({})\n👁 {}",
                preview_text(&h.thought),
                h.action,
                preview_tool_input(&h.action_input),
                preview_text(&h.observation)
            )
        })
        .collect();

    let outcome = if final_answer.is_empty() {
        format!("Completed {iterations_used} step(s). Files touched: {}",
            if files_modified.is_empty() { "none".to_string() } else { files_modified.join(", ") })
    } else {
        final_answer
    };

    ctx.record_system_event(
        "adk_coding_agent_done",
        Some(preview_text(&outcome)),
        Some(if verified { "DONE" } else { "FOLLOWUP_NEEDED" }),
        Some(format!("files={} verified={verified} dry_run={dry_run}", files_modified.len())),
    );

    Ok(CodingAgentResponse {
        outcome,
        files_modified,
        iterations_used,
        diff,
        verified,
        verification_output,
        trace,
        dry_run,
    })
}

// ── Context & planning ────────────────────────────────────────────────────────

async fn gather_project_context(ctx: &AgentContext, task: &str) -> String {
    let overview = ctx
        .call_tool("project_overview", json!({}))
        .await
        .ok()
        .and_then(|v| v.get("summary").and_then(Value::as_str).map(|s| s.to_string()))
        .unwrap_or_default();

    let search = ctx
        .call_tool("codebase_search", json!({ "query": task, "limit": 4 }))
        .await
        .ok()
        .map(|v| format_tool_output(&v))
        .unwrap_or_default();

    format!(
        "Project overview: {overview}\n\nRelevant codebase context:\n{search}"
    )
}

async fn make_plan(ctx: &AgentContext, task: &str, project_ctx: &str) -> String {
    let prompt = format!(
        "You are an expert software engineer. \
Plan the minimal steps to complete this coding task.\n\n\
Task: {task}\n\n\
{project_ctx}\n\n\
List 3-6 numbered steps. Be specific about which files to read or modify. \
Return only the numbered list, no extra prose.",
    );
    call_coding_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt)
        .await
        .unwrap_or_else(|_| "1. Read relevant files\n2. Make changes\n3. Verify".to_string())
}

// ── ReAct prompt ──────────────────────────────────────────────────────────────

fn describe_tools() -> String {
    let tools = [
        ("file_list", r#"{"path":"dir","max_depth":3}"#, "List files in a directory."),
        ("local_file_read", r#"{"path":"src/foo.rs"}"#, "Read a file's content."),
        ("file_write", r#"{"path":"src/foo.rs","content":"..."}"#, "Write full content to a file (use only when replacing the entire file)."),
        ("file_patch", r#"{"path":"src/foo.rs","hunks":[{"old_str":"fn foo() {","new_str":"fn foo() -> i32 {"}]}"#, "Apply surgical hunk-based edits. Fails atomically if any old_str is not found. Prefer over file_write for partial changes."),
        ("file_diff", r#"{"path":null}"#, "Show git diff of uncommitted changes."),
        ("shell_exec", r#"{"command":"cargo check"}"#, "Run a whitelisted shell command."),
        ("codebase_search", r#"{"query":"...","limit":5}"#, "Search codebase for relevant code."),
        ("symbol_search", r#"{"query":"function_name"}"#, "Search for a symbol by name."),
        ("call_graph_query", r#"{"symbol":"my_fn","hops":1}"#, "Look up callers and callees of a symbol in the call graph."),
        ("plan_execute", r#"{"steps":[{"tool":"file_patch","input":{...}},{"tool":"shell_exec","input":{"command":"cargo check"}}]}"#, "Execute multiple tool steps in sequence. Stops on first failure. Use to batch multi-file changes in one action."),
        ("git_status", r#"{}"#, "Show git status."),
        ("git_log", r#"{"limit":5}"#, "Show recent git commits."),
        ("memory_search", r#"{"query":"...","limit":3}"#, "Search past memories."),
    ];
    tools
        .iter()
        .map(|(name, example, desc)| format!("- `{name}({example})`: {desc}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_react_prompt(
    task: &str,
    project_ctx: &str,
    plan: &str,
    history: &[HistoryEntry],
    tool_list: &str,
    dry_run: bool,
) -> String {
    let dry_run_note = if dry_run {
        "\nNOTE: Running in DRY-RUN mode. For file_write actions, pass `\"dry_run\": true` in \
action_input. Files will NOT be written to disk; the agent will report what would change.\n"
    } else {
        ""
    };

    let history_block = if history.is_empty() {
        String::new()
    } else {
        let entries: Vec<String> = history
            .iter()
            .map(|h| {
                format!(
                    "Thought: {}\nAction: {}\nAction Input: {}\nObservation: {}",
                    h.thought,
                    h.action,
                    serde_json::to_string(&h.action_input).unwrap_or_default(),
                    h.observation
                )
            })
            .collect();
        format!("\n## Previous steps\n{}\n", entries.join("\n---\n"))
    };

    format!(
        r#"You are Sirin, a local AI Coding Agent.
{dry_run_note}
## Task
{task}

## Plan
{plan}

## Project context
{project_ctx}
{history_block}
## Available tools
{tool_list}

## Instructions
Decide the next single action to take.

**Tool preferences:**
- Prefer `file_patch` over `file_write` whenever you are making partial changes to an existing file. `file_patch` is surgical and safe — it fails atomically if the context doesn't match, preventing accidental corruption.
- Use `plan_execute` when the task requires changes to multiple files — batch all the `file_patch` (and optionally a final `shell_exec`) calls into one `plan_execute` action to complete the work in a single step.
- Use `call_graph_query` to understand callers and callees before modifying a function.

Respond with ONLY valid JSON in this exact format (no markdown fences):
{{
  "thought": "your reasoning about what to do next",
  "action": "tool_name or DONE",
  "action_input": {{}},
  "final_answer": "summary of what you accomplished (only when action is DONE)"
}}

If you have finished ALL steps in the plan and the task is complete, set action to "DONE".
"#
    )
}

// ── Step parsing ──────────────────────────────────────────────────────────────

struct ReactStep {
    thought: String,
    action: String,
    action_input: Value,
    final_answer: Option<String>,
}

fn parse_react_step(raw: &str) -> ReactStep {
    // Strip markdown code fences if present.
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    if let Ok(v) = serde_json::from_str::<Value>(cleaned) {
        let thought = v
            .get("thought")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let action = v
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("DONE")
            .to_string();
        let action_input = v.get("action_input").cloned().unwrap_or(json!({}));
        let final_answer = v
            .get("final_answer")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        return ReactStep { thought, action, action_input, final_answer };
    }

    // Fallback: LLM didn't produce valid JSON — stop the loop gracefully.
    ReactStep {
        thought: format!("(LLM response could not be parsed as JSON) raw={}", preview_text(raw)),
        action: "DONE".to_string(),
        action_input: json!({}),
        final_answer: Some(format!("Could not parse LLM step. Raw output: {}", preview_text(raw))),
    }
}

// ── Post-processing ───────────────────────────────────────────────────────────

async fn verify_build(ctx: &AgentContext) -> (bool, Option<String>) {
    match ctx
        .call_tool("shell_exec", json!({ "command": "cargo check" }))
        .await
    {
        Ok(v) => {
            let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);
            let output = format!(
                "exit_code={}\nstdout={}\nstderr={}",
                v.get("exit_code").and_then(Value::as_i64).unwrap_or(-1),
                v.get("stdout").and_then(Value::as_str).unwrap_or(""),
                v.get("stderr").and_then(Value::as_str).unwrap_or(""),
            );
            (success, Some(output))
        }
        Err(e) => (false, Some(format!("shell_exec error: {e}"))),
    }
}

async fn get_diff(ctx: &AgentContext) -> Option<String> {
    ctx.call_tool("file_diff", json!({}))
        .await
        .ok()
        .and_then(|v| v.get("diff").and_then(Value::as_str).map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
}

// ── History entry ─────────────────────────────────────────────────────────────

struct HistoryEntry {
    thought: String,
    action: String,
    action_input: Value,
    observation: String,
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn format_tool_output(v: &Value) -> String {
    match v {
        Value::String(s) => s.chars().take(800).collect(),
        Value::Array(arr) => arr
            .iter()
            .take(10)
            .map(|item| item.as_str().unwrap_or(&item.to_string()).chars().take(120).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            let s = serde_json::to_string_pretty(other).unwrap_or_default();
            s.chars().take(800).collect()
        }
    }
}

fn preview_tool_input(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    s.chars().take(60).collect()
}

fn preview_text(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}

// ── Public runner ─────────────────────────────────────────────────────────────

pub async fn run_coding_via_adk(
    task: String,
    dry_run: bool,
    tracker: Option<TaskTracker>,
) -> CodingAgentResponse {
    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("coding_request")
        .with_optional_tracker(tracker)
        .with_metadata("agent", "coding_agent");

    let input = json!(CodingRequest { task: task.clone(), max_iterations: None, dry_run });
    match runtime.run(&CodingAgent, ctx, input).await {
        Ok(v) => serde_json::from_value(v).unwrap_or_else(|_| CodingAgentResponse {
            outcome: "Completed (response parse error)".to_string(),
            ..Default::default()
        }),
        Err(e) => {
            sirin_log!("[coding_agent] run failed: {e}");
            CodingAgentResponse {
                outcome: format!("Error: {e}"),
                ..Default::default()
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_react_step_valid_json() {
        let raw = r#"{"thought":"read the file","action":"local_file_read","action_input":{"path":"src/main.rs"}}"#;
        let step = parse_react_step(raw);
        assert_eq!(step.action, "local_file_read");
        assert!(step.final_answer.is_none());
    }

    #[test]
    fn parse_react_step_done() {
        let raw = r#"{"thought":"done","action":"DONE","action_input":{},"final_answer":"Applied fix."}"#;
        let step = parse_react_step(raw);
        assert_eq!(step.action, "DONE");
        assert_eq!(step.final_answer.as_deref(), Some("Applied fix."));
    }

    #[test]
    fn parse_react_step_bad_json_gracefully_stops() {
        let step = parse_react_step("not valid json at all");
        assert_eq!(step.action, "DONE");
        assert!(step.final_answer.is_some());
    }

    #[test]
    fn parse_react_step_strips_markdown_fences() {
        let raw = "```json\n{\"thought\":\"t\",\"action\":\"DONE\",\"action_input\":{}}\n```";
        let step = parse_react_step(raw);
        assert_eq!(step.action, "DONE");
    }
}
