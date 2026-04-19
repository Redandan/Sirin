# MCP Server Architecture

> Source: `src/mcp_server.rs`
> Cross-references: [./test_runner.md](./test_runner.md),
> [./multi_agent.md](./multi_agent.md), [./browser.md](./browser.md),
> [../MCP_API.md](../MCP_API.md)

---

## 1. Purpose

Sirin exposes its internals as an MCP (Model Context Protocol) HTTP endpoint
on port **7700** (configurable via `SIRIN_RPC_PORT`).  Any MCP-compatible
client — Claude Desktop, external AI agents, the `sirin_call` CLI, or
curl — can connect and call tools to:

- Drive the browser (`browser_exec`, `page_state`)
- Trigger and poll AI test runs (`run_adhoc_test`, `get_test_result`)
- Inspect and manage the multi-agent squad (`agent_enqueue`, `agent_queue_status`)
- Search Sirin's memory, read configs, trigger research
- Consult another Claude Code session across repos (`consult`, `supervised_run`)

All capabilities are exposed as named tools over a single `POST /mcp` endpoint
using the JSON-RPC wire format defined by the MCP specification (2025-03-26
Streamable HTTP transport).

**Claude Desktop config:**

```json
{
  "mcpServers": {
    "sirin": {
      "url": "http://127.0.0.1:7700/mcp"
    }
  }
}
```

---

## 2. Endpoints

### `POST /mcp`

The sole endpoint.  Accepts a JSON-RPC 2.0 envelope:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "<method>", "params": { ... } }
```

Returns a JSON-RPC 2.0 response:

```json
// success
{ "jsonrpc": "2.0", "id": 1, "result": { ... } }

// error
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32603, "message": "..." } }
```

### Supported methods

| Method | Description |
|---|---|
| `initialize` | MCP handshake — registers client identity, returns server capabilities |
| `tools/list` | Returns the full tool catalogue with JSON schemas |
| `tools/call` | Invoke a named tool with its arguments |
| `notifications/initialized` | No-op acknowledgement (no response body) |

Any other method returns `"Method not found: <method>"`.

### Transport layer (`mcp_router()`, line 135)

The axum Router wraps every request with `TimeoutLayer`:

```rust
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
```

If a handler hangs (e.g., Chrome CDP connection dies), axum returns HTTP 408
and closes the socket cleanly instead of leaving a CLOSE_WAIT zombie.
Long-running operations (`run_adhoc_test`, `run_test_async`) return a `run_id`
immediately and are unaffected by this timeout.

---

## 3. Tools List (28 tools)

### Test runner

| Tool | Description |
|---|---|
| `list_tests` | List all YAML test goals under `config/tests/` (optional `tag` filter) |
| `run_test_async` | Spawn a YAML-defined test asynchronously; return `run_id` |
| `run_test_batch` | Spawn N tests in parallel (max 8 tabs); return N `run_id`s |
| `run_adhoc_test` | Run a test from URL + goal inline — no YAML needed |
| `persist_adhoc_run` | Promote a passed ad-hoc run to a permanent `config/tests/*.yaml` |
| `get_test_result` | Poll a `run_id` for status: queued / running / passed / failed / timeout |
| `get_screenshot` | Fetch failure screenshot (base64 PNG) for a `run_id` |
| `get_full_observation` | Fetch untruncated browser observation for a specific step |
| `list_recent_runs` | Historical test runs (optional `test_id` filter, default 20, max 100) |
| `list_fixes` | Auto-fix history (claude_session spawns and outcomes) |

### Browser control

| Tool | Description |
|---|---|
| `browser_exec` | Imperative browser actions — see Section 5 for full action list |
| `page_state` | Snapshot: URL + title + ax_tree + JPEG screenshot + console + network |

### Diagnostics

| Tool | Description |
|---|---|
| `config_diagnostics` | Config health report (LLM, router, vision, Chrome, Claude CLI) |
| `diagnose` | Full snapshot: version, git commit, uptime, recent errors, GitHub issue template |

### Memory / skills / integrations

| Tool | Description |
|---|---|
| `memory_search` | FTS5 search over Sirin's SQLite memory store |
| `skill_list` | List all skills (built-in + YAML dynamic) |
| `teams_pending` | Teams pending-reply draft list |
| `teams_approve` | Approve a Teams draft (triggers send) |
| `trigger_research` | Publish `ResearchRequested` event for the researcher pipeline |

### Multi-agent squad

| Tool | Description |
|---|---|
| `agent_team_status` | PM / Engineer / Tester session IDs + resume commands |
| `agent_team_task` | Synchronous task: PM → Engineer → PM review loop (blocking, up to MAX_ITER) |
| `agent_team_test` | Trigger test cycle: Tester runs cargo test, Engineer fixes failures |
| `agent_send` | Send a message directly to one role, get reply |
| `agent_reset` | Reset one or all role sessions (clear conversation history) |
| `agent_enqueue` | Push a task into the JSONL queue (priority support) |
| `agent_queue_status` | View all tasks with status, description preview, result preview |
| `agent_start_worker` | Start N background worker threads consuming the queue |
| `agent_clear_completed` | Remove Done/Failed tasks from the queue |

### Cross-repo delegation

| Tool | Description |
|---|---|
| `consult` | Ask another Claude Code session in a specified repo, return its advice |
| `supervised_run` | Run Claude Code in a repo with auto-supervision (policy: auto or consult) |

---

## 4. Tool Dispatcher

### `handle_tools_call()` (`mcp_server.rs:655`)

Dispatches `tools/call` by matching `params.name`:

```rust
match name {
    "list_tests"           => call_list_tests(arguments).map(wrap_json),
    "run_test_async"       => call_run_test_async(arguments).map(wrap_json),
    "run_test_batch"       => call_run_test_batch(arguments).map(wrap_json),
    "run_adhoc_test"       => call_run_adhoc_test(arguments).map(wrap_json),
    "persist_adhoc_run"    => call_persist_adhoc_run(arguments).map(wrap_json),
    "get_test_result"      => call_get_test_result(arguments).map(wrap_json),
    "get_screenshot"       => call_get_screenshot(arguments).map(wrap_json),
    "get_full_observation" => call_get_full_observation(arguments).map(wrap_json),
    "list_recent_runs"     => call_list_recent_runs(arguments).map(wrap_json),
    "list_fixes"           => call_list_fixes(arguments).map(wrap_json),
    "config_diagnostics"   => call_config_diagnostics().map(wrap_json),
    "diagnose"             => Ok(wrap_json(crate::diagnose::snapshot())),
    "browser_exec"         => call_browser_exec(arguments, user_agent).await.map(wrap_json),
    "page_state"           => call_page_state(arguments).await.map(wrap_json),
    "consult"              => call_consult(arguments).map(wrap_json),
    "supervised_run"       => call_supervised_run(arguments).map(wrap_json),
    "agent_team_status"    => call_agent_team_status(arguments).map(wrap_json),
    "agent_team_task"      => call_agent_team_task(arguments).map(wrap_json),
    "agent_team_test"      => call_agent_team_test(arguments).map(wrap_json),
    "agent_send"           => call_agent_send(arguments).map(wrap_json),
    "agent_reset"          => call_agent_reset(arguments).map(wrap_json),
    "agent_enqueue"        => call_agent_enqueue(arguments).map(wrap_json),
    "agent_queue_status"   => call_agent_queue_status().map(wrap_json),
    "agent_start_worker"   => call_agent_start_worker(arguments).map(wrap_json),
    "agent_clear_completed"=> call_agent_clear_completed().map(wrap_json),
    _ => {}   // falls through to text-only tools
}
// Text-only tools (memory_search, skill_list, teams_*, trigger_research)
```

### Return format

All structured tools call `wrap_json(payload)`:

```rust
fn wrap_json(payload: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&payload) }] })
}
```

Text-only tools return the same envelope with a raw string instead of a
serialised object.

### Blocking helper (`blocking()`, line 107)

CPU-bound / blocking-I/O calls are dispatched via tokio's blocking pool:

```rust
async fn blocking<T, F>(label: &'static str, f: F) -> Result<T, String>
where F: FnOnce() -> Result<T, String> + Send + 'static
{
    match tokio::task::spawn_blocking(f).await {
        Ok(inner) => inner,
        Err(e) if e.is_panic() => Err(format!("{label}: handler panicked")),
        Err(e) => Err(format!("{label}: join error: {e}")),
    }
}
```

Panics in handlers are converted to `Err` rather than aborting the process.
This is safe because Cargo.toml leaves `panic = "unwind"` (the default) in
`[profile.release]`.

### Async-only path

Two tools bypass `spawn_blocking` and run directly in the tokio async context:

- `browser_exec` with `action = "screenshot_analyze"` — requires an LLM
  call (`crate::llm::analyze_screenshot`) which is already async.
- `page_state` — aggregates multiple async operations with `blocking()` for
  the CPU-bound parts internally.

---

## 5. `browser_exec` Action Sub-Dispatcher

`browser_exec` is a single MCP tool that fans out to 45+ browser actions
dispatched by the `action` field (`mcp_server.rs:1425`).

### Navigation

| Action | Required | Description |
|---|---|---|
| `goto` | `target` = URL | Navigate to URL; opens browser if closed |
| `url` | — | Return current URL |
| `title` | — | Return page title |
| `close` | — | Close browser |

### Screenshots & vision

| Action | Required | Description |
|---|---|---|
| `screenshot` | — | Full-page PNG (base64) |
| `screenshot_analyze` | `target` = analysis prompt | Vision LLM analysis of current screenshot |

### DOM interaction

| Action | Required | Description |
|---|---|---|
| `click` | `target` = CSS selector | Click element |
| `click_point` | `x`, `y` | Click by screen coordinates (for Flutter/CanvasKit) |
| `type` | `target` = selector, `text` | Type into element |
| `read` | `target` = selector | Read element text |
| `eval` | `target` = JS expression | Evaluate JavaScript, return result |
| `attr` | `target` = selector, `text` = attr name | Read DOM attribute |
| `exists` | `target` = selector | Boolean: element present? |
| `wait` | `target` = selector | Poll until element appears (default 5000ms) |
| `scroll` | `timeout` = px | Scroll page by Y pixels |
| `key` | `target` = key name | Press keyboard key |
| `set_viewport` | `width`, `height` | Set viewport size, `device_scale`, `mobile` |

### Console & network capture

| Action | Required | Description |
|---|---|---|
| `console` | `timeout` = limit (default 20) | Recent console messages |
| `network` | `timeout` = limit (default 20) | Recent captured network requests |

### Accessibility tree (AX)

| Action | Required | Description |
|---|---|---|
| `enable_a11y` | — | Bootstrap Flutter semantics, return node count |
| `ax_tree` | `include_ignored` (optional) | Full AX tree as structured nodes |
| `ax_find` | `role` and/or `name` | Find node by role+name; `limit>1` returns array; `scroll=true` for Flutter lists |
| `ax_value` | `backend_id` | Read text value of AX node by backend ID |
| `ax_click` | `backend_id` | Click AX node by backend ID |
| `ax_focus` | `backend_id` | Focus AX node by backend ID |
| `ax_type` | `backend_id`, `text` | Type into AX node |
| `ax_type_verified` | `backend_id`, `text` | Type and verify (read-back assertion) |

### AX snapshots & diffs

| Action | Required | Description |
|---|---|---|
| `ax_snapshot` | `id` (optional) | Capture AX tree snapshot, return snapshot ID |
| `ax_diff` | `before_id`, `after_id` | Structural diff between two snapshots |
| `wait_for_ax_change` | `baseline_id`, `timeout` | Block until AX tree differs from baseline |

### Condition-based waits

| Action | Required | Description |
|---|---|---|
| `wait_for_url` | `target` = URL substring or `/regex/` | Wait until URL matches (default 10000ms) |
| `wait_for_ax_ready` | `min_nodes` (default 20), `timeout` | Wait until AX tree has ≥ N nodes |
| `wait_for_network_idle` | `idle_ms` (default 500), `timeout` (default 15000) | Wait until no network activity |

### Assertions

| Action | Required | Description |
|---|---|---|
| `assert_ax_contains` | `target` = text | Passes if text found anywhere in AX tree |
| `assert_url_matches` | `target` = URL substring or `/regex/` | Passes if current URL matches |

### Test isolation & tab management

| Action | Required | Description |
|---|---|---|
| `clear_state` | — | Clear cookies, localStorage, sessionStorage |
| `wait_new_tab` | `timeout` (default 10000) | Block until a new tab opens |
| `wait_request` | `target` = URL substring | Block until a matching network request fires |

### Multi-session (named tabs)

| Action | Required | Description |
|---|---|---|
| `list_sessions` | — | List named sessions: `session_id`, `tab_index`, `url` |
| `close_session` | `target` = session_id | Close a named tab |

All actions accept an optional `session_id` parameter to route to a named tab.

### Extension probes (experimental)

| Action | Description |
|---|---|
| `ext_status` | Sirin Companion extension connection status |
| `ext_url` | Authoritative URL from extension (falls back to CDP) |
| `ext_tabs` | Extension-reported tab list |

---

## 6. Squad Tools

The nine `agent_*` tools bridge MCP callers into `src/multi_agent/`.

### `agent_enqueue` → `queue::enqueue_with_priority()`

```rust
fn call_agent_enqueue(args: Value) -> Result<Value, String> {
    let priority = args.get("priority")...unwrap_or(50);
    let id = crate::multi_agent::queue::enqueue_with_priority(task, priority);
    Ok(json!({ "task_id": id, "status": "queued", ... }))
}
```

Returns `task_id` (millisecond timestamp string).  Workers started with
`agent_start_worker` pick it up automatically.

### `agent_start_worker` → `worker::spawn_n()`

```rust
fn call_agent_start_worker(args: Value) -> Result<Value, String> {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.compare_exchange(false, true, ...).is_ok() {
        let n = args.get("n")...clamp(1, 8);
        crate::multi_agent::worker::spawn_n(&cwd, n);
        Ok(json!({ "status": "started", "workers": n }))
    } else {
        Ok(json!({ "status": "already_running" }))
    }
}
```

`STARTED` is a separate `AtomicBool` inside `call_agent_start_worker`, distinct
from the one in `worker.rs`.  Second call always returns `"already_running"`.

### `agent_queue_status` → `queue::list_all()`

Returns a summary with `total / queued / running / done / failed` counts and
per-task previews (description truncated to 80 bytes, result to 120 bytes).

### `resolve_cwd()` — default working directory

All squad tools call:

```rust
fn resolve_cwd(args: &Value) -> String {
    args["cwd"].as_str()
        .map(|s| s.to_string())
        .or_else(|| crate::claude_session::repo_path("sirin"))
        .unwrap_or_else(|| ".".to_string())
}
```

`claude_session::repo_path("sirin")` walks `CLAUDE_REPO_PATH` / git discovery
to find the Sirin repository root.  MCP callers can override with an explicit
`cwd` argument.

### Tool ↔ module mapping

| MCP Tool | Rust call chain |
|---|---|
| `agent_enqueue` | `queue::enqueue_with_priority()` |
| `agent_queue_status` | `queue::list_all()` |
| `agent_start_worker` | `worker::spawn_n()` |
| `agent_clear_completed` | `queue::clear_completed()` |
| `agent_team_status` | `multi_agent::get_or_init()` → `team.status()` |
| `agent_team_task` | `team.assign_task()` (blocking, up to MAX_ITER=5) |
| `agent_team_test` | `team.test_cycle()` |
| `agent_send` | `team.{pm,engineer,tester}.send()` |
| `agent_reset` | `team.reset_role()` |

See [./multi_agent.md](./multi_agent.md) for the full squad internals.

---

## 7. Port Retry (7700 → 7701 → 7702)

The MCP server binds with a three-attempt retry loop (in `src/rpc_server.rs`):

```
try port 7700
  → EADDRINUSE → wait 2s → try 7701
  → EADDRINUSE → wait 2s → try 7702
  → EADDRINUSE → fatal error
```

The bound port is stored and exposed in the UI status bar and
`diagnose` output.  `SIRIN_RPC_PORT` overrides the base port.  The `sirin_call`
CLI reads the active port from the same config path so it targets whichever port
was actually bound.

---

## 8. Client Identity & Session Store

### Problem

The original implementation used a single `static Mutex<String>` for
`CURRENT_CLIENT_ID`.  Two concurrent clients calling `initialize`
simultaneously would overwrite each other's identity, misattributing audit log
entries.

### Solution (`CLIENT_SESSIONS`, line 75)

```rust
static CLIENT_SESSIONS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
```

Maps each client's `User-Agent` header to a `"name@version"` string derived
from its `initialize` params.  Concurrent clients with different UAs maintain
independent identity records.

**`remember_client_id(user_agent, client_id)`** — called by `handle_initialize()`.

**`resolve_client_id(user_agent)`** — called per-request in `call_browser_exec()`.
Falls back to the raw `User-Agent` string when the client hasn't called
`initialize` yet (e.g., curl probes, `sirin_call` ad-hoc calls).

### AuthZ integration

`browser_exec` passes `client_id` to `crate::authz::decide()` for per-client
action authorization.  Decisions: `Allow / Deny / Ask / AskWithLearn`.  `Ask`
pauses execution for 30 seconds, emitting an `authz_ask` event to the monitor
UI so a human can approve or deny interactively.

---

## 9. Schema Generation

All tool schemas are defined as **inline `json!()` literals** inside
`handle_tools_list()` (line 226).  There is no code-generation step.

### Schema structure per tool

```json
{
    "name": "tool_name",
    "description": "...",
    "inputSchema": {
        "type": "object",
        "properties": {
            "field": { "type": "string/number/boolean/array/object", "description": "..." }
        },
        "required": ["field1", "field2"]
    }
}
```

### Notable schema patterns

- **Optional numeric defaults** — documented in `description` (e.g.,
  `"limit: default 5"`); not enforced by the schema itself.
- **Enum constraints** — `agent_send.role` uses `"enum": ["pm","engineer","tester"]`
  so MCP clients can offer a dropdown.
- **Nested fixture schema** — `run_adhoc_test.fixture` has a full two-level
  `setup / cleanup` array schema with required `action` fields.
- **Integer bounds** — `agent_enqueue.priority` uses `"minimum": 0, "maximum": 255`.

---

## 10. Refactor Candidates

At ~1900 lines, `mcp_server.rs` is the largest single file in the project.

### Suggested split

| Proposed module | Contents |
|---|---|
| `src/mcp_server/mod.rs` | Router, `mcp_handler`, `dispatch`, `blocking`, client-session store |
| `src/mcp_server/schema.rs` | `handle_tools_list()` with all JSON schema definitions |
| `src/mcp_server/browser.rs` | `call_browser_exec()`, `call_page_state()` (~350 lines) |
| `src/mcp_server/test_runner.rs` | All `call_run_*`, `call_get_*`, `call_list_*` test handlers |
| `src/mcp_server/squad.rs` | All `call_agent_*` handlers + `resolve_cwd()` |
| `src/mcp_server/misc.rs` | `call_memory_search`, `call_skill_list`, `call_teams_*`, `call_trigger_research`, `call_consult`, `call_supervised_run`, `call_config_diagnostics` |

### Current pain points

1. **`browser_exec` is 350+ lines** — the action `match` block from line 1425
   to ~1756 handles 45 cases inline.  Extracting to `src/mcp_server/browser.rs`
   would make each group (nav, ax, sessions, assertions) easier to navigate.

2. **Schema and implementation co-located** — adding a new tool requires
   editing both `handle_tools_list()` and `handle_tools_call()` at opposite
   ends of the file.  A trait-based registration pattern would keep each tool
   self-contained, but adds indirection.

3. **`call_supervised_run` ~60 lines** — the event-callback pattern is verbose.
   Moving to `claude_session.rs` would reduce it to a thin wrapper here.

4. **Tests at file bottom** — `test_runner_mcp_tests` (lines 1847–1900)
   are only for the test-runner subset.  A module split would allow per-module
   tests.

### Non-issues

- The `blocking()` helper and `wrap_json()` are lightweight enough to stay in
  `mod.rs` after a split.
- The 180-second `MCP_REQUEST_TIMEOUT` constant belongs at the router level
  and should not move to a sub-module.
