# MCP Client Architecture

> Source: `src/mcp_client.rs`
> Cross-references: [./mcp_server.md](./mcp_server.md),
> [../MCP_API.md](../MCP_API.md)

---

## 1. Purpose

While `src/mcp_server.rs` makes Sirin's tools available *to* external clients,
`src/mcp_client.rs` goes in the other direction: Sirin connects *outbound* to
external MCP servers and surfaces their tools to its internal agents.

```
  Claude Desktop                        agora-trading
  (MCP client)                          (MCP server)
       |                                     |
       | POST /mcp                           |
       v                                     |
  +-----------------+                        |
  |  Sirin :7700    |                        |
  |  mcp_server.rs  |                        |
  |                 |  POST http://...       |
  |  mcp_client.rs  | ───────────────────> ──|
  |                 | <── tools, results ─── |
  +-----------------+
```

This makes Sirin a **fully bidirectional MCP node**: it is simultaneously a
provider (offering browser, test_runner, squad tools) and a consumer (calling
trading, ops, and other domain servers).

Discovered external tools are registered in Sirin's `ToolRegistry` under
namespaced names (`mcp_{server}_{tool}`) so any internal agent can call them
transparently — the same as calling `web_navigate` or `run_test`.

---

## 2. Transport

Only **HTTP** transport is implemented.  Each `McpServerEntry` specifies a `url`
field pointing to the remote `POST /mcp` endpoint:

```yaml
servers:
  - name: agora-trading
    url: "http://localhost:3001/mcp"
    enabled: true
```

Sirin uses a single shared `reqwest::Client` (kept in `McpClientState`) for all
outbound requests.  reqwest manages connection pooling and keep-alive internally.

**stdio transport is not implemented.**  The module was designed with HTTP-only
in mind; stdio (spawning a child process and speaking JSON-RPC over its
stdin/stdout) would require a separate transport path.  All known Sirin
integrations (agora-trading, agora-ops) run as HTTP servers on localhost.

---

## 3. Server Registry

### Config file

```
%LOCALAPPDATA%\Sirin\config\mcp_servers.yaml   (production)
./config/mcp_servers.yaml                       (#[cfg(test)])
```

Schema (`McpServersConfig`, `mcp_client.rs:26`):

```yaml
servers:
  - name: agora-trading        # Identifies the server; used as name prefix
    url: "http://localhost:3001/mcp"
    enabled: true              # Omit or false to skip without removing the entry
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | String | required | Becomes the `{server}` part of `mcp_{server}_{tool}` |
| `url` | String | required | Full HTTP URL to the remote `POST /mcp` endpoint |
| `enabled` | bool | `true` | Set `false` to skip without deleting the entry |

### Current config (`config/mcp_servers.yaml`)

As of writing, the servers list is empty (`servers: []`) with the agora-trading
entry commented out as a reference:

```yaml
servers: []
  # - name: agora-trading
  #   url: "http://localhost:3001/mcp"
  #   enabled: true
```

To activate, uncomment the entry and restart Sirin.

---

## 4. Tool Discovery

### Init flow (`mcp_client.rs:116`)

`init()` is called once at startup before agents are created:

```
McpServersConfig::load()          read YAML from platform::config_path()
       |
       v
for each enabled server:
  discover_tools(http, server)    POST initialize, POST tools/list
       |
       +-- Ok(tools)  ──────────> extend all_tools list, log OK
       +-- Err(e)     ──────────> log error, skip server (non-fatal)
       |
       v
McpClientState.tools = all_tools  write into OnceLock<Arc<RwLock<...>>>
DISCOVERED.set(all_tools)         synchronous snapshot for ToolRegistry
```

Failures are logged and skipped — a single unreachable server does not prevent
Sirin from starting or using other servers.

### Handshake sequence (`discover_tools`, `mcp_client.rs:169`)

For each server, two sequential requests are made:

1. **`initialize`** — MCP handshake.  Sirin identifies as
   `{ name: "sirin", version: "<CARGO_PKG_VERSION>" }`.
   The response is discarded; the call is required by the MCP spec before
   `tools/list` is allowed.

2. **`tools/list`** — Returns the server's tool catalogue.  Sirin extracts
   `result.tools[*].{ name, description, inputSchema }` and constructs
   `ExternalTool` entries.

### `ExternalTool` struct (`mcp_client.rs:63`)

```rust
pub struct ExternalTool {
    pub server_name:  String,   // from config (e.g. "agora-trading")
    pub tool_name:    String,   // as reported by the server (e.g. "getBalance")
    pub description:  String,   // passed to LLM as tool description
    pub input_schema: Value,    // JSON Schema from `inputSchema` field
    pub server_url:   String,   // stored for routing call_tool() later
}

impl ExternalTool {
    pub fn registry_name(&self) -> String {
        format!("mcp_{}_{}", self.server_name, self.tool_name)
        // e.g. "mcp_agora-trading_getBalance"
    }
}
```

### ToolRegistry registration

After `init()`, the calling code (startup sequence) iterates
`get_discovered_tools()` and registers each tool in Sirin's `ToolRegistry`.
The handler closure captures `server_url` and `tool_name`, and calls
`mcp_client::call_tool()` when the agent invokes the tool.

`get_discovered_tools()` (`mcp_client.rs:108`) provides a **synchronous**
snapshot from `DISCOVERED: OnceLock<Vec<ExternalTool>>` — necessary because
ToolRegistry building happens in synchronous context where `.await` is not
available.

---

## 5. Tool Call Flow

When an agent uses `mcp_agora-trading_getBalance({"currency":"USDT"})`:

```
Agent (LLM output)
  | "mcp_agora-trading_getBalance"
  v
ToolRegistry.dispatch()
  | handler registered for this name
  v
mcp_client::call_tool(server_url, "getBalance", {"currency":"USDT"})
  |
  +-- POST server_url
  |   { jsonrpc:"2.0", id:1, method:"tools/call",
  |     params:{ name:"getBalance", arguments:{currency:"USDT"} } }
  |   timeout: 120s
  |
  +-- Response: { result: { content: [{ type:"text", text:"..." }] } }
  |   extract all text items, join with "\n"
  |   return Ok({ result: "<text>" })
  |
  +-- Response: { error: { message: "..." } }
  |   return Err("MCP tool error: ...")
  |
  +-- HTTP / parse failure
      return Err("MCP call failed (...): ...")
```

**Timeout**: 120 seconds (`mcp_client.rs:274`).
This is separate from — and shorter than — the MCP server's own 180 s
`TimeoutLayer`.  Tool calls that exceed 120 s return an error to the agent.

**Result unwrapping**: MCP tool results arrive as
`{ content: [{ type: "text", text: "..." }] }`.  `call_tool` joins all text
blocks into a single string wrapped as `{ result: "<text>" }`.  If the server
returns a non-content result (raw JSON), it is returned as-is.

---

## 6. LLM Context Injection

`describe_tools_for_prompt()` (`mcp_client.rs:320`) generates a compact summary
of all external tools for injection into agent system prompts:

```
## External MCP Tools
- `mcp_agora-trading_getBalance({"currency":"..."})`: Get wallet balance
- `mcp_agora-trading_listStrategies({"status":"..."})`: List trading strategies
```

The schema example is capped at 4 properties via `compact_schema_example()` to
keep the injected context short.  Type mapping: `number`/`integer` -> `0`,
`boolean` -> `true`, everything else -> `"..."`.

This function is `async` — it reads from the `RwLock`-guarded `McpClientState`.

---

## 7. Why Both Client + Server in One Binary

Sirin is a **full MCP peer**, not just a provider or just a consumer:

| Role | Module | Direction | Who connects |
|---|---|---|---|
| **Provider** | `src/mcp_server.rs` | Inbound `POST /mcp :7700` | Claude Desktop, sirin_call CLI, external agents |
| **Consumer** | `src/mcp_client.rs` | Outbound `POST {url}` | agora-trading, agora-ops, and any other local MCP server |

This separation of concerns means:

- The **provider** surface is stable (fixed port, fixed tool names).
- The **consumer** surface is dynamic (tools auto-discovered at startup).
- No circular dependency: `mcp_server.rs` does not import `mcp_client.rs`.
  The ToolRegistry wires them together at startup.

A typical session where a Claude Desktop user asks "What is the current BTC
balance?" would flow: Claude Desktop -> `mcp_server.rs` (browser/agent tools) ->
agent invokes `mcp_agora-trading_getBalance` -> `mcp_client.rs` -> agora-trading
HTTP server -> result back to agent -> response to Claude Desktop.

---

## 8. Known Limits / Future Work

### HTTP only — no stdio transport

All servers must expose an HTTP `POST /mcp` endpoint.  Servers that are only
available as CLI tools (stdio transport, e.g. `npx @modelcontextprotocol/...`)
cannot be used without wrapping them in an HTTP shim.

### No reconnect / re-discovery

Tools are discovered once at startup and stored in `DISCOVERED`.  If:
- An external server restarts, tool calls will resume once the server is back
  (reqwest retries at TCP level), but new tools added to the server are invisible
  until Sirin restarts.
- An external server goes down permanently, its tools remain registered but
  every call returns `Err("MCP call failed...")`.

A `reload_mcp_tools()` admin endpoint would fix both without restarting Sirin.

### No SSE / streaming consumer

The MCP 2025-03-26 spec supports Server-Sent Events for streaming results.
Sirin's client reads the full HTTP response body before returning — streaming
tool outputs are not supported.

### No authentication

The client sends no credentials.  All configured servers are assumed to be
on localhost (or a trusted private network).  A future `auth:` YAML field
(Bearer token / API key header) would be needed for remote or multi-tenant
servers.

### Discovery failures are silent at runtime

If a server was reachable at startup but goes down later, `call_tool()` returns
an error string to the agent rather than triggering a notification.  The agent
may retry or surface the error, but no alerting or health-check mechanism exists.
