//! Prompt construction and response parsing for the ReAct loop.
//!
//! - Project context gathering (project_overview + codebase_search + task-named
//!   file hints).
//! - Plan generation (one-shot router LLM call).
//! - Per-iteration ReAct prompt assembly (task + plan + context + history +
//!   tools + instructions), with automatic project_ctx trimming to stay within
//!   `MAX_PROMPT_CHARS`.
//! - JSON response parsing (`ReactStep`) with a fallback that re-prompts
//!   instead of falsely DONE-ing on malformed output.

use serde_json::{json, Value};

use crate::adk::AgentContext;

use super::helpers::{
    build_task_named_file_context, extract_json_body, extract_path_hints_from_task,
    format_tool_output, is_read_only_analysis_task, preview_text,
};
use super::verdict::HistoryEntry;

/// Soft character limit for a single ReAct prompt.  LM Studio at 32K context ≈
/// 128K chars (4 chars/token estimate).  We leave headroom for the LLM output.
const MAX_PROMPT_CHARS: usize = 20_000;

// ── Project context ──────────────────────────────────────────────────────────

pub(super) async fn gather_project_context(ctx: &AgentContext, task: &str) -> String {
    let overview = ctx
        .call_tool("project_overview", json!({}))
        .await
        .ok()
        .and_then(|v| {
            v.get("summary")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    let path_hints = extract_path_hints_from_task(task);
    let search_query = path_hints
        .first()
        .cloned()
        .unwrap_or_else(|| task.chars().take(60).collect());
    let search = ctx
        .call_tool(
            "codebase_search",
            json!({ "query": search_query, "limit": 4 }),
        )
        .await
        .ok()
        .map(|v| format_tool_output(&v))
        .unwrap_or_default();

    let explicit_file_context = build_task_named_file_context(&path_hints);

    format!(
        "Project overview: {overview}\n\nRelevant codebase context:\n{search}{explicit_file_context}"
    )
}

// ── Planning ─────────────────────────────────────────────────────────────────

pub(super) async fn make_plan(ctx: &AgentContext, task: &str, project_ctx: &str) -> String {
    let prompt = format!(
        "You are an expert software engineer. \
Plan the minimal steps to complete this coding task.\n\n\
Task: {task}\n\n\
{project_ctx}\n\n\
List 3-6 numbered steps. Be specific about which files to read or modify. \
Return only the numbered list, no extra prose.",
    );
    // Plan generation is a lightweight step-list task — use the router (local)
    // LLM to save remote quota for the actual ReAct coding iterations.
    crate::llm::call_prompt(ctx.http.as_ref(), &crate::llm::shared_router_llm(), prompt)
        .await
        .unwrap_or_else(|_| "1. Read relevant files\n2. Make changes\n3. Verify".to_string())
}

// ── ReAct prompt ─────────────────────────────────────────────────────────────

pub(super) fn build_react_prompt(
    task: &str,
    project_ctx: &str,
    plan: &str,
    history: &[&HistoryEntry],
    tool_list: &str,
    dry_run: bool,
) -> String {
    let dry_run_note = if dry_run {
        "\nNOTE: Running in DRY-RUN mode. For file_write, file_patch, and plan_execute actions \
pass `\"dry_run\": true` in action_input. Files will NOT be written to disk; the agent will \
report what would change.\n"
    } else {
        ""
    };
    let analysis_mode_note = if is_read_only_analysis_task(task) {
        "\nREAD-ONLY ANALYSIS MODE: inspect the most relevant 2-4 files, then return `DONE` with a concise evidence-based summary that cites the file paths you used. Avoid repeating the same reads/searches once the answer is clear.\n"
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

    // Token budget: estimate static sections and trim project_ctx if necessary
    // so the full prompt stays under MAX_PROMPT_CHARS.  Keeps the LLM from
    // receiving a truncated prompt silently when context is large.
    let static_budget = task.len() + plan.len() + tool_list.len()
        + history_block.len() + 800 /* boilerplate */;
    let ctx_budget = MAX_PROMPT_CHARS.saturating_sub(static_budget);
    let project_ctx_trimmed: String = project_ctx.chars().take(ctx_budget.max(400)).collect();

    format!(
        r#"You are Sirin, a local AI Coding Agent.
{dry_run_note}
## Task
{task}

## Plan
{plan}

## Project context
{project_ctx_trimmed}
{history_block}
## Available tools
{tool_list}

## Instructions
Decide the next single action to take.{analysis_mode_note}

**Tool preferences:**
- Prefer `file_patch` over `file_write` whenever you are making partial changes to an existing file. `file_patch` is surgical and safe — it fails atomically if the context doesn't match, preventing accidental corruption.
- Before any write, first confirm the exact target path with `local_file_read`, `file_list`, or `codebase_search`. In Rust projects, `foo.rs` may actually live at `foo/mod.rs`.
- If the task explicitly names a file path, inspect that path first and treat the resolved file as primary evidence.
- If a read/patch returns `not found` or `old_str` mismatch, do NOT say `DONE`. Re-discover the real path, re-read the latest file, and then retry the surgical patch.
- If the requested behavior is already present, cite the confirming file/path evidence in `final_answer` instead of forcing another edit.
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

// ── Step parsing ─────────────────────────────────────────────────────────────

pub(super) struct ReactStep {
    pub(super) thought: String,
    pub(super) action: String,
    pub(super) action_input: Value,
    pub(super) final_answer: Option<String>,
    pub(super) parse_error: bool,
}

pub(super) fn parse_react_step(raw: &str) -> ReactStep {
    let cleaned = extract_json_body(raw);

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
        return ReactStep {
            thought,
            action,
            action_input,
            final_answer,
            parse_error: false,
        };
    }

    // Fallback: LLM didn't produce valid JSON — request another iteration
    // instead of falsely treating the task as complete.
    ReactStep {
        thought: format!(
            "(LLM response could not be parsed as JSON) raw={}",
            preview_text(raw)
        ),
        action: "INVALID_JSON".to_string(),
        action_input: json!({}),
        final_answer: None,
        parse_error: true,
    }
}
