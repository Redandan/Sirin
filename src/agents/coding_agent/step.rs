//! Shared ReAct step execution helpers.
//!
//! Both [`super::react::run_react_iterations`] (the main ReAct loop) and
//! [`super::verify::run_fix_iterations`] (the post-verify auto-fix loop)
//! need to:
//! 1. Dispatch a tool call with a given action name + input.
//! 2. Extract the set of files the tool just modified (so auto-commit /
//!    rollback / the followup reason can see them).
//! 3. Format the result as either a large excerpt (file_read) or a compact
//!    observation (everything else).
//! 4. Enrich error observations with a path-hint when it looks like a
//!    resolution mistake.
//!
//! `execute_tool` below captures steps 1–4 in one place.  The main ReAct
//! loop layers its `file_read_cache` on top; the fix loop calls it directly.

use serde_json::Value;

use crate::adk::AgentContext;

use super::helpers::{format_tool_output, format_tool_output_large, maybe_enrich_tool_error};

/// Result of executing one ReAct tool call.
pub(super) struct ToolExecution {
    /// Formatted observation string to append to history and show the LLM on
    /// the next iteration.  Errors are prefixed with `"ERROR:"` so callers
    /// can detect failures without parsing JSON back out.
    pub(super) observation: String,
    /// File paths the tool claims to have modified.  Caller merges with its
    /// own running set (callers never want duplicates, so dedup is on them).
    pub(super) files_modified: Vec<String>,
}

/// Dispatch `action` against the registry, format the result, and extract any
/// modified file paths.  No caching — callers that want to dedupe reads layer
/// their own cache before calling this.
pub(super) async fn execute_tool(
    ctx: &AgentContext,
    action: &str,
    input: Value,
    is_file_read: bool,
) -> ToolExecution {
    match ctx.call_tool(action, input).await {
        Ok(v) => {
            let files_modified = extract_modified_paths(action, &v);
            let observation = if is_file_read {
                format_tool_output_large(&v)
            } else {
                format_tool_output(&v)
            };
            ToolExecution {
                observation,
                files_modified,
            }
        }
        Err(e) => {
            let raw = format!("ERROR: {e}");
            ToolExecution {
                observation: maybe_enrich_tool_error(action, raw),
                files_modified: Vec::new(),
            }
        }
    }
}

/// Scan a tool-call result for file paths that were written / patched.
///
/// - `file_write` / `file_patch` → single `path` field on the result.
/// - `plan_execute` → iterate `results[].result.path` for each sub-step.
/// - anything else → empty list.
pub(super) fn extract_modified_paths(action: &str, v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if action == "file_write" || action == "file_patch" {
        if let Some(path) = v.get("path").and_then(Value::as_str) {
            out.push(path.to_string());
        }
    }
    if action == "plan_execute" {
        if let Some(results) = v.get("results").and_then(Value::as_array) {
            for r in results {
                if let Some(result) = r.get("result") {
                    if let Some(path) = result.get("path").and_then(Value::as_str) {
                        out.push(path.to_string());
                    }
                }
            }
        }
    }
    out
}
