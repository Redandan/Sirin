//! MCP tool registry — Option B from #257 proposal.
//!
//! Replaces the 3-place split (schema literal in `handle_tools_list`,
//! dispatch arm in `handle_tools_call`, handler `fn call_X`) with one
//! [`ToolDef`] entry per tool.  Adding a tool becomes a single
//! `register_tool!` call near its handler.
//!
//! ## Coexistence with the legacy path
//!
//! This module is additive — existing match-arm dispatched tools keep
//! working unchanged.  `handle_tools_list` and `handle_tools_call` in
//! `mcp_server.rs` consult the registry FIRST, then fall through to
//! the legacy literals / arms for un-migrated tools.
//!
//! Migrating a tool means:
//! 1. Delete its schema literal from `handle_tools_list`.
//! 2. Delete its match arm from `handle_tools_call`.
//! 3. Add a `register_tool!(...)` call inside [`seed_default`] below.
//!
//! Goal #1 from the proposal — "1 place to add a tool" — is met for
//! migrated tools.  Schema↔handler compile-time link (goal #2) is
//! NOT met by this design; that needs a proc macro (Option C).
//!
//! ## Handler signature variants
//!
//! Sirin's 92 tool handlers use 5 distinct signatures (the 6th —
//! `browser_exec` with `user_agent` — stays in the legacy path because
//! it's the sole authz exception).  [`ToolHandler`] is a tagged union
//! over the 5 sane variants; non-capturing closures coerce zero-arg
//! handlers (`fn () -> ...`) into the `(Value) -> ...` variants.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use serde_json::{json, Value};

// ── Public types ────────────────────────────────────────────────────────────

/// One tool's definition: name + schema + dispatch handler.  Inserted
/// into the global registry once at startup.
pub struct ToolDef {
    pub name:         &'static str,
    pub description: &'static str,
    /// JSON Schema for the tool's `arguments` payload.  Lives as a
    /// `Value` (not a struct) so we don't have to standardise the
    /// schema shape across all 92 tools — each registration can pass
    /// whatever shape the existing literal had.
    pub input_schema: Value,
    pub handler:      ToolHandler,
}

impl ToolDef {
    /// Render the tool definition into the JSON shape that
    /// `tools/list` returns.
    pub fn to_json(&self) -> Value {
        json!({
            "name":        self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        })
    }
}

/// Tagged union over Sirin's tool-handler signature variants.
///
/// **Why not one canonical signature**: rewriting 92 handlers down to
/// `async fn(Value) -> Result<Value, String>` would touch ~75 sync fns
/// and mass-async-ify them — a bigger churn than the registry refactor
/// itself.  The enum lets us migrate handlers as-is.
pub enum ToolHandler {
    /// Sync handler returning structured JSON.  Most common variant.
    SyncJson(fn(Value) -> Result<Value, String>),

    /// Async handler returning structured JSON.  Used by handlers that
    /// hit the LLM / KB / network.  The fn-pointer must be a NON-capturing
    /// closure (or a top-level fn) — closure variables cannot be smuggled
    /// into a `static`.
    AsyncJson(fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'static>>),

    /// Sync handler returning plain text (memory_search, skill_list, etc).
    /// The text wrap (`{"content":[{"type":"text","text":...}]}`) is
    /// applied uniformly inside [`ToolHandler::invoke`].
    SyncText(fn(Value) -> Result<String, String>),

    /// Async text handler.
    AsyncText(fn(Value) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>>),
}

impl ToolHandler {
    /// Run the handler and return the MCP-envelope-shaped JSON
    /// (`{"content":[...]}` for text mode, raw JSON wrapped via
    /// `wrap_json` for structured mode).  Always returns the same
    /// envelope shape regardless of handler variant — callers don't
    /// need to know which variant ran.
    pub async fn invoke(&self, args: Value) -> Result<Value, String> {
        match self {
            Self::SyncJson(f)  => f(args).map(wrap_json),
            Self::AsyncJson(f) => f(args).await.map(wrap_json),
            Self::SyncText(f)  => f(args).map(wrap_text),
            Self::AsyncText(f) => f(args).await.map(wrap_text),
        }
    }
}

// MCP-envelope helpers, mirrored from `mcp_server::wrap_json`.  Local
// copies so `mcp_registry` doesn't depend on private mcp_server items.
fn wrap_json(payload: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
        }]
    })
}

fn wrap_text(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }]
    })
}

// ── Global registry ─────────────────────────────────────────────────────────

/// Sorted by tool name so `tools/list` output is stable across runs.
type RegistryMap = BTreeMap<&'static str, ToolDef>;

static REGISTRY: OnceLock<RegistryMap> = OnceLock::new();

/// Initialise + return the global registry.  Idempotent — first caller
/// wins; subsequent callers get the cached map.
pub fn registry() -> &'static RegistryMap {
    REGISTRY.get_or_init(|| {
        let mut m = BTreeMap::new();
        seed_default(&mut m);
        m
    })
}

/// Look up a tool by name.  Returns `None` for un-migrated tools
/// (the caller should fall through to the legacy match path).
pub fn get(name: &str) -> Option<&'static ToolDef> {
    registry().get(name)
}

/// Iterate the registry's tool definitions in name order.
pub fn iter() -> impl Iterator<Item = &'static ToolDef> {
    registry().values()
}

/// `true` when the registry has at least one tool.
#[allow(dead_code)]
pub fn is_populated() -> bool {
    !registry().is_empty()
}

// ── Seed: hand-listed registrations ─────────────────────────────────────────
//
// Each migrated tool gets one entry here.  The order doesn't matter (the
// BTreeMap sorts on insert), but grouping by domain makes review easier.
//
// Migrations land in batches — see #257 for the running checklist.

fn seed_default(m: &mut RegistryMap) {
    // Domain: skills + teams (text-mode handlers).
    m.insert("skill_list", ToolDef {
        name:         "skill_list",
        description: "列出 Sirin 所有可用技能（含 YAML 動態技能）。",
        input_schema: json!({"type": "object", "properties": {}}),
        // Wrap the zero-arg handler into a `fn(Value) -> Result<String, String>`
        // shape via a non-capturing closure.  Closures without captures coerce
        // to fn pointers, which is what the enum variant requires.
        handler:      ToolHandler::SyncText(|_args| Ok(crate::mcp_server::call_skill_list())),
    });

    m.insert("teams_pending", ToolDef {
        name:         "teams_pending",
        description: "取得 Teams 待確認回覆草稿列表。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncText(|_args| Ok(crate::mcp_server::call_teams_pending())),
    });

    // Domain: discovery (structured-JSON handler with no args).
    m.insert("discovery_status", ToolDef {
        name:         "discovery_status",
        description: "查詢 discovery crawler 的目前狀態（NotRun / Crawling / Done + 元數據）。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_discovery_status()),
    });
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_seeded_tools() {
        // Seeded tools must be in the registry.
        assert!(get("skill_list").is_some());
        assert!(get("teams_pending").is_some());
        assert!(get("discovery_status").is_some());

        // Un-migrated tools must NOT be in the registry yet.
        assert!(get("run_test_async").is_none());
        assert!(get("memory_search").is_none());
    }

    #[test]
    fn tool_def_renders_to_expected_json_shape() {
        let def = get("skill_list").expect("seeded");
        let json = def.to_json();
        assert_eq!(json["name"],        "skill_list");
        assert_eq!(json["description"], "列出 Sirin 所有可用技能（含 YAML 動態技能）。");
        assert!(json["inputSchema"].is_object());
    }

    #[test]
    fn iter_returns_alphabetised_by_name() {
        // BTreeMap preserves insertion-key order — our seeded entries
        // are skill_list / teams_pending / discovery_status in source
        // order, but iter() returns them sorted: discovery_status comes
        // first alphabetically.
        let names: Vec<&str> = iter().map(|d| d.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "iter() must be alphabetical");
    }

    #[tokio::test]
    async fn sync_text_handler_round_trip_through_envelope() {
        let def = get("skill_list").expect("seeded");
        let envelope = def.handler.invoke(json!({})).await.expect("call");
        // The envelope must be `{"content":[{"type":"text","text":...}]}`.
        assert_eq!(envelope["content"][0]["type"], "text");
        assert!(envelope["content"][0]["text"].is_string());
    }

    #[test]
    fn wrap_json_helper_produces_text_envelope_with_pretty_payload() {
        let payload = json!({"k": "v"});
        let env = wrap_json(payload);
        let text = env["content"][0]["text"].as_str().unwrap();
        // Pretty printer adds a newline after the opening brace.
        assert!(text.contains("\"k\": \"v\""));
    }
}
