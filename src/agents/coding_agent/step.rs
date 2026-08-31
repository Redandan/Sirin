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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Issue #263 Phase 3: tests for extract_modified_paths.
    // Pure JSON shape parser — covers each action class the function knows.

    #[test]
    fn extract_paths_from_file_write() {
        let v = json!({"path": "src/main.rs", "bytes_written": 1234});
        assert_eq!(
            extract_modified_paths("file_write", &v),
            vec!["src/main.rs"]
        );
    }

    #[test]
    fn extract_paths_from_file_patch() {
        let v = json!({"path": "Cargo.toml", "patched": true});
        assert_eq!(extract_modified_paths("file_patch", &v), vec!["Cargo.toml"]);
    }

    #[test]
    fn extract_paths_from_plan_execute_collects_all_steps() {
        let v = json!({
            "results": [
                {"step": 1, "result": {"path": "src/a.rs"}},
                {"step": 2, "result": {"path": "src/b.rs"}},
                {"step": 3, "result": {"unrelated": "x"}},     // no path → skipped
                {"step": 4, "result": {"path": "tests/c.rs"}},
            ]
        });
        let paths = extract_modified_paths("plan_execute", &v);
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "tests/c.rs"]);
    }

    #[test]
    fn extract_paths_returns_empty_for_unknown_action() {
        let v = json!({"path": "src/main.rs"});
        // file_read / browser_exec / etc. — should NOT report writes
        assert!(extract_modified_paths("file_read", &v).is_empty());
        assert!(extract_modified_paths("browser_exec", &v).is_empty());
        assert!(extract_modified_paths("unknown_action", &v).is_empty());
    }

    #[test]
    fn extract_paths_safe_when_path_field_missing() {
        let v = json!({"bytes_written": 99});
        assert!(extract_modified_paths("file_write", &v).is_empty());
        assert!(extract_modified_paths("file_patch", &v).is_empty());
    }

    #[test]
    fn extract_paths_safe_when_path_field_wrong_type() {
        // path as number / array / null instead of string — must not panic.
        let v = json!({"path": 42});
        assert!(extract_modified_paths("file_write", &v).is_empty());
        let v = json!({"path": null});
        assert!(extract_modified_paths("file_write", &v).is_empty());
        let v = json!({"path": ["a","b"]});
        assert!(extract_modified_paths("file_write", &v).is_empty());
    }

    #[test]
    fn extract_paths_plan_execute_handles_missing_results_array() {
        let v = json!({"unrelated": true});
        assert!(extract_modified_paths("plan_execute", &v).is_empty());
        let v = json!({"results": "not an array"});
        assert!(extract_modified_paths("plan_execute", &v).is_empty());
        let v = json!({"results": []});
        assert!(extract_modified_paths("plan_execute", &v).is_empty());
    }

    #[test]
    fn extract_paths_plan_execute_handles_step_without_result_object() {
        let v = json!({
            "results": [
                {"step": 1},                                  // no result key
                {"step": 2, "result": null},                  // result null
                {"step": 3, "result": {"path": "src/ok.rs"}}, // valid
            ]
        });
        // Only step 3's path should be collected.
        assert_eq!(
            extract_modified_paths("plan_execute", &v),
            vec!["src/ok.rs"]
        );
    }
}
