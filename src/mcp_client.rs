//! MCP Client — connect to external MCP servers and proxy their tools.
//!
//! At startup, reads `config/mcp_servers.yaml` and connects to each enabled server.
//! Discovered tools are registered into the Sirin [`ToolRegistry`] so agents can
//! call them transparently (e.g. `mcp_agora-trading_getBalance`).
//!
//! # Config format
//!
//! ```yaml
//! servers:
//!   - name: agora-trading
//!     url: "http://localhost:3001/mcp"
//!     enabled: true
//! ```

use std::sync::{Arc, OnceLock};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

// ── Config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServersConfig {
    #[serde(default)]
    pub servers: Vec<McpServerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    pub url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl McpServersConfig {
    pub fn load() -> Self {
        let path = "config/mcp_servers.yaml";
        match std::fs::read_to_string(path) {
            Ok(content) => serde_yaml::from_str(&content).unwrap_or_else(|e| {
                eprintln!("[mcp_client] Failed to parse {path}: {e}");
                Self::default()
            }),
            Err(_) => {
                // File doesn't exist — that's fine, no external servers configured.
                Self::default()
            }
        }
    }
}

impl Default for McpServersConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
        }
    }
}

// ── Discovered tool metadata ─────────────────────────────────────────────────

/// A tool discovered from an external MCP server.
#[derive(Debug, Clone)]
pub struct ExternalTool {
    /// MCP server name (from config).
    pub server_name: String,
    /// Original tool name as reported by the server.
    pub tool_name: String,
    /// Tool description for the LLM.
    pub description: String,
    /// JSON Schema for the tool's input (from `inputSchema`).
    pub input_schema: Value,
    /// The full URL of the MCP server endpoint.
    pub server_url: String,
}

impl ExternalTool {
    /// The name used in Sirin's ToolRegistry: `mcp_{server}_{tool}`.
    pub fn registry_name(&self) -> String {
        format!("mcp_{}_{}", self.server_name, self.tool_name)
    }
}

// ── Global state ─────────────────────────────────────────────────────────────

/// Process-wide MCP client state.
struct McpClientState {
    http: Client,
    /// All discovered tools from all connected servers.
    tools: Vec<ExternalTool>,
}

static STATE: OnceLock<Arc<RwLock<McpClientState>>> = OnceLock::new();

fn state() -> Arc<RwLock<McpClientState>> {
    Arc::clone(STATE.get_or_init(|| {
        Arc::new(RwLock::new(McpClientState {
            http: Client::new(),
            tools: Vec::new(),
        }))
    }))
}

/// Discovered tools cached for synchronous access by ToolRegistry builders.
/// Set by `init()`, read by `get_discovered_tools()`.
static DISCOVERED: OnceLock<Vec<ExternalTool>> = OnceLock::new();

/// Get tools discovered during init (synchronous, for ToolRegistry building).
pub fn get_discovered_tools() -> &'static [ExternalTool] {
    DISCOVERED.get().map(|v| v.as_slice()).unwrap_or(&[])
}

// ── Initialization ───────────────────────────────────────────────────────────

/// Connect to all configured MCP servers and discover their tools.
/// Call this once at startup, before agents are used.
pub async fn init() -> Vec<ExternalTool> {
    let config = McpServersConfig::load();
    let enabled: Vec<_> = config.servers.into_iter().filter(|s| s.enabled).collect();

    if enabled.is_empty() {
        return Vec::new();
    }

    eprintln!(
        "[mcp_client] Connecting to {} MCP server(s)...",
        enabled.len()
    );

    let st = state();
    let guard = st.read().await;
    let http = guard.http.clone();
    drop(guard);

    let mut all_tools = Vec::new();

    for server in &enabled {
        match discover_tools(&http, server).await {
            Ok(tools) => {
                eprintln!(
                    "[mcp_client] ✓ {} — discovered {} tool(s): {}",
                    server.name,
                    tools.len(),
                    tools
                        .iter()
                        .map(|t| t.tool_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                all_tools.extend(tools);
            }
            Err(e) => {
                eprintln!("[mcp_client] ✗ {} — connection failed: {e}", server.name);
            }
        }
    }

    // Store discovered tools in global state.
    let mut guard = st.write().await;
    guard.tools = all_tools.clone();
    drop(guard);

    // Also set the synchronous cache for ToolRegistry builders.
    let _ = DISCOVERED.set(all_tools.clone());

    all_tools
}

/// Discover tools from a single MCP server by calling `tools/list`.
async fn discover_tools(
    http: &Client,
    server: &McpServerEntry,
) -> Result<Vec<ExternalTool>, String> {
    // First: initialize the MCP connection.
    let init_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "sirin",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }
    });

    http.post(&server.url)
        .json(&init_body)
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    // Then: list available tools.
    let list_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    let resp = http
        .post(&server.url)
        .json(&list_body)
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    let json: Value = resp.json().await.map_err(|e| format!("JSON error: {e}"))?;

    // Extract tools array from the response.
    let tools_array = json
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut tools = Vec::new();
    for tool_val in tools_array {
        let name = tool_val
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let desc = tool_val
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let schema = tool_val
            .get("inputSchema")
            .cloned()
            .unwrap_or(json!({"type": "object", "properties": {}}));

        if !name.is_empty() {
            tools.push(ExternalTool {
                server_name: server.name.clone(),
                tool_name: name,
                description: desc,
                input_schema: schema,
                server_url: server.url.clone(),
            });
        }
    }

    Ok(tools)
}

// ── Tool invocation ──────────────────────────────────────────────────────────

/// Call a tool on an external MCP server.
///
/// This is invoked by the ToolRegistry handler when an agent uses an
/// `mcp_{server}_{tool}` tool.
pub async fn call_tool(server_url: &str, tool_name: &str, arguments: Value) -> Result<Value, String> {
    let st = state();
    let guard = st.read().await;
    let http = guard.http.clone();
    drop(guard);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments
        }
    });

    let resp = http
        .post(server_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("MCP call failed ({server_url}): {e}"))?;

    let json: Value = resp.json().await.map_err(|e| format!("MCP response parse error: {e}"))?;

    // Check for JSON-RPC error.
    if let Some(err) = json.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("MCP tool error: {msg}"));
    }

    // Extract the result.  MCP tool results come as `{ content: [{ type: "text", text: "..." }] }`.
    let result = json.get("result").cloned().unwrap_or(json!(null));

    // Try to extract text from MCP content format.
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let text: String = content
            .iter()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return Ok(json!({ "result": text }));
        }
    }

    // Return raw result if not in MCP content format.
    Ok(result)
}

// ── Public helpers ───────────────────────────────────────────────────────────

/// Get the list of all discovered external tools.
pub async fn list_tools() -> Vec<ExternalTool> {
    let st = state();
    let guard = st.read().await;
    guard.tools.clone()
}

/// Build a text description of all MCP tools for inclusion in LLM prompts.
/// Format: `- mcp_server_tool({"param":"..."}): description`
pub async fn describe_tools_for_prompt() -> String {
    let tools = list_tools().await;
    if tools.is_empty() {
        return String::new();
    }
    let mut lines = vec!["\n## External MCP Tools".to_string()];
    for tool in &tools {
        let example = compact_schema_example(&tool.input_schema);
        lines.push(format!(
            "- `{name}({example})`: {desc}",
            name = tool.registry_name(),
            desc = tool.description,
        ));
    }
    lines.join("\n")
}

/// Generate a compact JSON example from a JSON Schema `properties` object.
fn compact_schema_example(schema: &Value) -> String {
    let props = match schema.get("properties").and_then(Value::as_object) {
        Some(p) => p,
        None => return "{}".to_string(),
    };
    let pairs: Vec<String> = props
        .iter()
        .take(4) // limit to avoid huge examples
        .map(|(k, v)| {
            let typ = v.get("type").and_then(Value::as_str).unwrap_or("string");
            let placeholder = match typ {
                "number" | "integer" => "0".to_string(),
                "boolean" => "true".to_string(),
                _ => format!("\"...\""),
            };
            format!("\"{k}\":{placeholder}")
        })
        .collect();
    format!("{{{}}}", pairs.join(","))
}
