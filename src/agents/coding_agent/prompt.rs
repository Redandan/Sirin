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

// Issue #256 — typed prompt args for plan generation.
pub(super) struct PlanPromptArgs<'a> {
    pub(super) task:        &'a str,
    pub(super) project_ctx: &'a str,
}

impl<'a> PlanPromptArgs<'a> {
    pub(super) fn render(&self) -> String {
        format!(
            "You are an expert software engineer. \
Plan the minimal steps to complete this coding task.\n\n\
Task: {task}\n\n\
{project_ctx}\n\n\
List 3-6 numbered steps. Be specific about which files to read or modify. \
Return only the numbered list, no extra prose.",
            task = self.task,
            project_ctx = self.project_ctx,
        )
    }
}

pub(super) async fn make_plan(ctx: &AgentContext, task: &str, project_ctx: &str) -> String {
    let prompt = PlanPromptArgs { task, project_ctx }.render();
    // Plan generation is a lightweight step-list task — use the router (local)
    // LLM to save remote quota for the actual ReAct coding iterations.
    crate::llm::call_prompt(ctx.http.as_ref(), &crate::llm::shared_router_llm(), prompt)
        .await
        .unwrap_or_else(|_| "1. Read relevant files\n2. Make changes\n3. Verify".to_string())
}

// ── ReAct prompt (typed, Issue #256) ─────────────────────────────────────────
//
// Issue #256 — typed prompt args. Adding/renaming a field is a compile-time
// failure at every call site instead of a silent `{var}` literal in the
// rendered prompt.  `format!` itself rejects unknown named args at compile
// time — so missing fields in the template OR missing args in the struct
// surface immediately.  See `tests::react_prompt_snapshot_*` for behavioural
// pinning.

pub(super) struct ReactPromptArgs<'a> {
    pub(super) task:        &'a str,
    pub(super) project_ctx: &'a str,
    pub(super) plan:        &'a str,
    pub(super) history:     &'a [&'a HistoryEntry],
    pub(super) tool_list:   &'a str,
    pub(super) dry_run:     bool,
}

impl<'a> ReactPromptArgs<'a> {
    pub(super) fn render(&self) -> String {
        let dry_run_note = if self.dry_run {
            "\nNOTE: Running in DRY-RUN mode. For file_write, file_patch, and plan_execute actions \
pass `\"dry_run\": true` in action_input. Files will NOT be written to disk; the agent will \
report what would change.\n"
        } else {
            ""
        };
        let analysis_mode_note = if is_read_only_analysis_task(self.task) {
            "\nREAD-ONLY ANALYSIS MODE: inspect the most relevant 2-4 files, then return `DONE` with a concise evidence-based summary that cites the file paths you used. Avoid repeating the same reads/searches once the answer is clear.\n"
        } else {
            ""
        };

        let history_block = if self.history.is_empty() {
            String::new()
        } else {
            let entries: Vec<String> = self.history
                .iter()
                .map(|h| {
                    format!(
                        "Thought: {thought}\nAction: {action}\n\
                         Action Input: {action_input}\nObservation: {observation}",
                        thought = h.thought,
                        action = h.action,
                        action_input =
                            serde_json::to_string(&h.action_input).unwrap_or_default(),
                        observation = h.observation,
                    )
                })
                .collect();
            format!("\n## Previous steps\n{}\n", entries.join("\n---\n"))
        };

        // Token budget: estimate static sections and trim project_ctx if necessary
        // so the full prompt stays under MAX_PROMPT_CHARS.
        let static_budget = self.task.len() + self.plan.len() + self.tool_list.len()
            + history_block.len() + 800 /* boilerplate */;
        let ctx_budget = MAX_PROMPT_CHARS.saturating_sub(static_budget);
        let project_ctx_trimmed: String =
            self.project_ctx.chars().take(ctx_budget.max(400)).collect();

        // Named format args — adding a new {field} requires adding the
        // matching = clause below, and removing a field flags every {field}
        // reference at compile time.
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
"#,
            dry_run_note       = dry_run_note,
            task               = self.task,
            plan               = self.plan,
            project_ctx_trimmed = project_ctx_trimmed,
            history_block      = history_block,
            tool_list          = self.tool_list,
            analysis_mode_note = analysis_mode_note,
        )
    }
}

/// Backwards-compatible thin wrapper — existing callers stay unchanged.
/// Prefer constructing [`ReactPromptArgs`] directly in new code.
pub(super) fn build_react_prompt(
    task: &str,
    project_ctx: &str,
    plan: &str,
    history: &[&HistoryEntry],
    tool_list: &str,
    dry_run: bool,
) -> String {
    ReactPromptArgs {
        task, project_ctx, plan, history, tool_list, dry_run,
    }.render()
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

// ── Tests ────────────────────────────────────────────────────────────────────
//
// Issue #256 — snapshot tests for the typed prompts.  The goal is to pin
// behavioural invariants (markers / sections present, correct branching on
// flags) rather than byte-for-byte equality, so a doc-comment tweak doesn't
// fail CI but a lost field reference does.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plan_prompt_includes_task_and_project_ctx() {
        let p = PlanPromptArgs {
            task:        "fix the auth bug",
            project_ctx: "axum + tower-http stack",
        }.render();
        assert!(p.contains("Task: fix the auth bug"));
        assert!(p.contains("axum + tower-http stack"));
        assert!(p.contains("3-6 numbered steps"));
        // No accidental unsubstituted {var} markers leaking through.
        assert!(!p.contains("{task}"));
        assert!(!p.contains("{project_ctx}"));
    }

    #[test]
    fn react_prompt_dry_run_branches_correctly() {
        let dry = ReactPromptArgs {
            task: "t", project_ctx: "ctx", plan: "p",
            history: &[], tool_list: "tl",
            dry_run: true,
        }.render();
        assert!(dry.contains("DRY-RUN mode"));

        let live = ReactPromptArgs {
            task: "t", project_ctx: "ctx", plan: "p",
            history: &[], tool_list: "tl",
            dry_run: false,
        }.render();
        assert!(!live.contains("DRY-RUN mode"));
    }

    #[test]
    fn react_prompt_renders_history_block_only_when_nonempty() {
        let empty = ReactPromptArgs {
            task: "t", project_ctx: "ctx", plan: "p",
            history: &[], tool_list: "tl", dry_run: false,
        }.render();
        assert!(!empty.contains("Previous steps"));

        let h = HistoryEntry {
            thought:      "looking around".into(),
            action:       "local_file_read".into(),
            action_input: json!({ "path": "src/main.rs" }),
            observation:  "// fn main()".into(),
            pinned:       false,
        };
        let entries: Vec<&HistoryEntry> = vec![&h];
        let with_history = ReactPromptArgs {
            task: "t", project_ctx: "ctx", plan: "p",
            history: &entries, tool_list: "tl", dry_run: false,
        }.render();
        assert!(with_history.contains("## Previous steps"));
        assert!(with_history.contains("looking around"));
        assert!(with_history.contains("local_file_read"));
        assert!(with_history.contains("src/main.rs"));
    }

    #[test]
    fn react_prompt_trims_project_ctx_when_static_budget_large() {
        // A massive task pushes the static budget close to MAX_PROMPT_CHARS,
        // forcing project_ctx to be trimmed (but never below 400 chars).
        let huge_task = "x".repeat(MAX_PROMPT_CHARS - 500);
        let huge_ctx = "y".repeat(50_000);
        let rendered = ReactPromptArgs {
            task: &huge_task, project_ctx: &huge_ctx, plan: "p",
            history: &[], tool_list: "tl", dry_run: false,
        }.render();
        // project_ctx was trimmed — should not contain the full 50k 'y's.
        let y_count = rendered.matches('y').count();
        assert!(y_count < 50_000, "got {y_count} y's — project_ctx not trimmed");
        // But trimmed to ≥ 400 (the floor).
        assert!(y_count >= 400, "got {y_count} y's — trimmed below floor");
    }

    #[test]
    fn react_prompt_analysis_mode_for_read_only_tasks() {
        // is_read_only_analysis_task triggers on Chinese 「分析」 / 「說明」
        // and English "explain" / "summar" / "inspect" / "review" / "read ".
        let analysis = ReactPromptArgs {
            task: "分析 src/llm/mod.rs 的設計",
            project_ctx: "ctx", plan: "p",
            history: &[], tool_list: "tl", dry_run: false,
        }.render();
        assert!(analysis.contains("READ-ONLY ANALYSIS MODE"));

        // A modify-task does not trigger analysis mode.
        let modify = ReactPromptArgs {
            task: "修改 src/main.rs 加 hello world",
            project_ctx: "ctx", plan: "p",
            history: &[], tool_list: "tl", dry_run: false,
        }.render();
        assert!(!modify.contains("READ-ONLY ANALYSIS MODE"));
    }
}
