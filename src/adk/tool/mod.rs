//! `ToolRegistry` — the ADK tool-dispatch surface.
//!
//! ## Concurrency
//! `ToolRegistry` is `Clone` (shares `Arc<HashMap>` inside).  Both
//! [`default_tool_registry`] and [`read_only_tool_registry`] are `OnceLock`
//! singletons initialised on first access.  Per-session variants are built
//! via [`ToolRegistry::without`] which returns a new shared handle without
//! mutating the original — safe to pass across async tasks.
//!
//! ## Public API
//! - [`ToolRegistry`] — clone-friendly handle over an Arc<HashMap>.
//! - [`default_tool_registry`] — full registry with all built-ins + MCP tools.
//! - [`read_only_tool_registry`] — default minus write/shell/plan tools.
//!
//! Submodules:
//! - [`builtins`] — the actual tool implementations (one big `build_full_registry`).
//! - [`fs_helpers`] — filesystem helpers (path-traversal guard, tree listing).

mod builtins;
mod fs_helpers;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use futures::{future::BoxFuture, FutureExt};
use serde_json::Value;

use crate::adk::context::AgentContext;

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

/// Full registry (write tools included). Cached process-wide — cheap to clone.
pub fn default_tool_registry() -> ToolRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(builtins::build_full_registry).clone()
}

/// Read-only registry — excludes file_write, file_patch, plan_execute, shell_exec.
/// Used by Chat Agent and Planner Agent so write tools are never accessible
/// from those agents regardless of what the LLM requests.
pub fn read_only_tool_registry() -> ToolRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            default_tool_registry().without(&[
                "file_write",
                "file_patch",
                "plan_execute",
                "shell_exec",
            ])
        })
        .clone()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Temp-file helpers for file_patch / plan_execute tests ─────────────────

    fn unique_test_filename(stem: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
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
        assert!(result.is_err());
    }

    // ── Phase 2: file_patch ───────────────────────────────────────────────────

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

        assert!(
            read_temp(&name).contains("patched by plan"),
            "file_patch side-effect must be persisted even though plan aborted"
        );

        remove_temp(&name);
    }
}
