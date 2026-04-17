# Sirin MCP API Reference

`http://127.0.0.1:7700/mcp` — MCP 2024-11-05 Streamable HTTP (JSON-RPC 2.0 over POST)

Port override: `SIRIN_RPC_PORT=<n>` env var at Sirin launch time (default `7700`).
When port 7700 is held by a zombie socket from a previously-killed Sirin, set
this to `7701` or similar.

Use this when Sirin is running and you want to drive it from an external agent
(Claude Code, Claude Desktop, custom scripts).

## sirin-call CLI

`sirin-call` is a thin Rust CLI wrapper that avoids bash shell-escaping pain
with CJK/Unicode payloads in curl.  Binary: `src/bin/sirin_call.rs`.

```bash
# Build once:
cargo build --release   # → target/release/sirin-call.exe

# key=value syntax (values auto-typed — numbers/booleans/arrays parsed as JSON):
sirin-call browser_exec action=url
sirin-call browser_exec action=ax_find role=button name=登入
sirin-call browser_exec action=wait_for_url target="#/home"

# Stdin JSON (Unicode-safe — no bash escaping needed):
echo '{"action":"ax_find","role":"button","name":"購買"}' | sirin-call browser_exec

# List available tools:
sirin-call --list

# Port override:
SIRIN_RPC_PORT=7701 sirin-call browser_exec action=url
```

Key=value pairs are auto-typed: numbers, booleans, arrays, and objects are
parsed as JSON first, falling back to plain string.  Bare strings don't need
quoting (`target=#/home` works).  Stdin JSON can supplement key=value args for
nested fields.

---

## Transport

All requests POST JSON-RPC 2.0 to `/mcp`:

```bash
curl -s http://127.0.0.1:7700/mcp -X POST \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Responses follow MCP's content format — tools returning structured data wrap
it as `{"content":[{"type":"text","text":"<stringified JSON>"}]}`.

## Registration (Claude Desktop / Claude Code)

```json
{
  "mcpServers": {
    "sirin": { "url": "http://127.0.0.1:7700/mcp" }
  }
}
```

## Methods

- `initialize` — MCP handshake (returns protocol version + server info)
- `tools/list` — enumerate tools
- `tools/call` — invoke one tool with JSON arguments

## Tool Catalog

### General (pre-existing)

#### `memory_search`
Search Sirin's memory (FTS5 SQLite).
```json
{"name":"memory_search","arguments":{"query":"...","limit":5}}
```

#### `skill_list`
List all YAML + built-in skills.
```json
{"name":"skill_list","arguments":{}}
```

#### `teams_pending` / `teams_approve`
Manage Teams pending draft queue.
```json
{"name":"teams_approve","arguments":{"id":"<pending_id>"}}
```

#### `trigger_research`
Kick off a research task.
```json
{"name":"trigger_research","arguments":{"topic":"...","url":"..."}}
```

---

### Test Runner (11 tools)

#### `list_tests`
Enumerate test YAMLs in `config/tests/`.
```json
{"name":"list_tests","arguments":{"tag":"smoke"}}
```
**Returns:** `{count, tests: [{id, name, url, goal, tags, max_iterations, timeout_secs}]}`

#### `run_test_async`
Fire-and-forget run of a YAML-defined test. Returns run_id immediately.
```json
{"name":"run_test_async","arguments":{
  "test_id":"wiki_smoke",
  "auto_fix": false
}}
```
**Returns:** `{run_id, test_id, auto_fix, status:"queued", poll_with:"get_test_result"}`

If `auto_fix: true`, failed runs trigger a `claude_session` spawn + verification
re-run (see the Failure Category table below).

#### `run_adhoc_test`
Test any URL without pre-creating a YAML goal. The most important tool for
external Claude Code sessions that receive requests against arbitrary URLs.
```json
{"name":"run_adhoc_test","arguments":{
  "url":"https://example.com",
  "goal":"Verify the home page loads with the expected title.",
  "success_criteria":["Page contains 'Example Domain'"],
  "locale":"en",
  "max_iterations":10,
  "timeout_secs":90,
  "browser_headless":false,
  "fixture":{
    "setup":[
      {"action":"goto","target":"https://example.com/login"},
      {"action":"click","target":"#guest-btn"},
      {"action":"wait","target":".dashboard"}
    ],
    "cleanup":[
      {"action":"clear_state"}
    ]
  }
}}
```
**`browser_headless` (optional):** `false` required for Flutter CanvasKit /
WebGL targets (they won't paint in headless Chrome → screenshots come back
black). Default reads `SIRIN_BROWSER_HEADLESS` env (itself defaulting to
`true`).

**`fixture` (optional):** `setup` steps run before the ReAct loop; `cleanup`
steps run unconditionally after the loop (even on timeout/error). Each step
supports `action`, `target`, `text`, and `timeout_ms` fields. A setup failure
aborts the run immediately with `error` status; cleanup errors are logged and
ignored.

Synthetic test_id format: `adhoc_<YYYYMMDD_HHMMSS_mmm>`. Results persist to
`test_runs` table with tag `adhoc`. Adhoc runs **skip** auto-fix verification
(no YAML to re-run).

#### `get_test_result`
Poll a run by id.
```json
{"name":"get_test_result","arguments":{"run_id":"run_..."}}
```
**Returns:** `{run_id, test_id, started_at, status, details}`

Status values:
- `queued` — spawned but not yet running
- `running` — details include `{step, current_action}`
- `passed` / `failed` / `timeout` / `error` — terminal states

For terminal states, `details` contains:
`{iterations, duration_ms, error, analysis, steps, has_screenshot, screenshot_error}`

#### `get_screenshot`
Fetch failure screenshot as base64 PNG.
```json
{"name":"get_screenshot","arguments":{"run_id":"run_..."}}
```
**Returns:** either `{mime, bytes_base64, size_bytes, url}` OR
`{bytes_base64: null, screenshot_error: "<reason>"}`.

#### `get_full_observation`
Retrieve full (un-truncated) tool output for a specific step. Useful when the
visible history shows `[truncated: ...]`.
```json
{"name":"get_full_observation","arguments":{
  "run_id":"run_...",
  "step": 3
}}
```
**Returns:** `{run_id, step, content, char_count}`

#### `list_recent_runs`
Query SQLite `test_runs` table. Omit `test_id` for cross-test view.
```json
{"name":"list_recent_runs","arguments":{"test_id":"login_smoke","limit":10}}
```
**Returns:** `{count, runs: [{id, test_id, started_at, duration_ms, status,
failure_category, ai_analysis, screenshot_path}]}`

#### `list_fixes`
Query auto-fix history.
```json
{"name":"list_fixes","arguments":{"test_id":"login_smoke"}}
```
**Returns:** `{count, fixes: [{id, test_id, run_id, category, triggered_at,
completed_at, outcome, claude_exit_code, claude_output, verification_run_id,
verified_at}]}`

Outcome values:
| Outcome | Meaning |
|---------|---------|
| `pending` | Spawned, claude_session in flight |
| `fix_attempted` | Claude returned exit=0, verification in progress |
| `verified` | Re-run passed after fix |
| `regressed` | Re-run still failed after fix → escalate |
| `failed` | claude_session exited non-zero (no verification attempted) |
| `skipped_dedupe` | Another fix was in flight, or 3 consecutive failures |

#### `config_diagnostics`
Self-check Sirin's LLM / router / vision / Chrome / Claude CLI.
```json
{"name":"config_diagnostics","arguments":{}}
```
**Returns:** `{count, errors, warnings, ok, issues: [{severity, category,
message, suggestion}], text_report}`

Use when tests are mysteriously failing across the board.

#### `page_state`
Single-call page snapshot: URL + title + condensed AX tree summary + last 5
console messages + JPEG thumbnail. Use instead of four separate `browser_exec`
calls when you need situational awareness without a specific assertion.
```json
{"name":"page_state","arguments":{}}
```
**Returns:**
```json
{
  "url":         "https://app.example.com/dashboard",
  "title":       "Dashboard – Acme",
  "ax_summary":  "button:Logout, text:Welcome Alice, button:Settings ...",
  "screenshot_b64": "<JPEG thumbnail — null if browser not open>",
  "console_recent": ["[warn] Slow network request", "..."]
}
```
Use for quick orientation ("what page are we on?") before deeper exploration.
`ax_summary` is a compact one-liner; for full tree use `browser_exec(ax_tree)`.

#### `browser_exec`
Imperative single-action browser control. Bypasses the full test goal flow.
```json
{"name":"browser_exec","arguments":{
  "action":"click",
  "target":"#submit-btn",
  "browser_headless":false
}}
```

**`browser_headless` (optional on every call):** overrides default on bind.
First call that sets this causes Sirin to launch (or relaunch) Chrome in
that mode. Subsequent calls without the flag reuse the current mode.
Changing the value between calls triggers a clean re-launch.

Supported `action` values:
| Action | Required args | Returns |
|--------|---------------|---------|
| `goto` | `target` (URL) | `{status, url}` |
| `screenshot` | — | `{mime, bytes_base64, size_bytes, url}` |
| **`screenshot_analyze`** | `target` (analysis prompt) | `{analysis, prompt}` — Gemini Vision reads the current page |
| `click` | `target` (selector) | `{status, selector}` |
| `type` | `target` (selector), `text` | `{status, selector, length}` |
| `read` | `target` (selector) | `{selector, text}` |
| `eval` | `target` (JS expr) | `{result}` |
| `wait` | `target` (selector), `timeout` (ms) | `{status, selector}` |
| `exists` | `target` (selector) | `{selector, exists}` |
| `attr` | `target` (selector), `text` (attr name) | `{selector, attribute, value}` |
| `scroll` | `timeout` (y pixels, default 300) | `{status, y}` |
| `key` | `target` (key name) | `{status, key}` |
| `console` | `timeout` (limit) | `{messages}` |
| `network` | `timeout` (limit) | `{requests}` |
| `url` | — | `{url}` |
| `title` | — | `{title}` |
| `close` | — | `{status}` |

**Accessibility tree actions** (literal text, no vision approximation —
required for K14/K15-style exact comparisons of $7376.80, error
messages, token counts):

| Action | Required args | Returns |
|--------|---------------|---------|
| `enable_a11y` | — | `{status}` — call before `ax_tree` on Flutter Canvas apps |
| `ax_tree` | — (optional `include_ignored`) | `{count, nodes:[{node_id, backend_id, role, name, value, description, child_ids}]}` |
| `ax_find` | `role` and/or `name` (substring, case-insensitive); optional `name_regex` (Rust regex, no implicit anchoring), `not_name_matches` (array of substrings to exclude), `limit` (int, default 1); scroll params: `scroll` (bool, default false), `scroll_max` (int, default 10) | `{found, count, nodes:[...], scrolled_times?}` — always an array; `scroll:true` scrolls the page up to `scroll_max` times when not found immediately |
| `ax_snapshot` | optional `id` (string key; auto-generated if omitted) | `{snapshot_id, count}` — stores current AX tree in memory keyed by `snapshot_id` |
| `ax_diff` | `before_id`, `after_id` | `{added:[{...}], removed:[{...}], changed:[{node_id, before_name, after_name}]}` — machine-readable delta between two snapshots |
| `wait_for_ax_change` | `baseline_id`; optional `timeout_ms` (default 5000) | `{changed:true, diff:{added,removed,changed}}` — blocks until tree differs from baseline; error on timeout |
| `ax_value` | `backend_id` | `{backend_id, text}` — literal `value \|\| name` |
| `ax_click` | `backend_id` | `{status, backend_id}` — clicks element centre via `DOM.getBoxModel` |
| `ax_focus` | `backend_id` | `{status, backend_id}` |
| `ax_type` | `backend_id`, `text` | `{status, backend_id, length}` — focus + insertText |
| `ax_type_verified` | `backend_id`, `text` | `{backend_id, typed, actual, matched}` — types, waits 300ms, reads back via a11y; `matched = actual.contains(typed)` |

**Robustness actions** (test isolation, race-free assertions, popup tabs):

| Action | Required args | Returns |
|--------|---------------|---------|
| `clear_state` | — | `{status:"cleared"}` — wipes cookies + localStorage + sessionStorage + IndexedDB + caches; doesn't close Chrome |
| `wait_request` | `target` (URL substring), optional `timeout` (ms, default 10000) | `{request: {url, method, status, req_body, body, ts}}` — auto-installs network capture; eliminates click-then-read race |
| `wait_new_tab` | optional `timeout` (ms, default 10000) | `{status, active_tab}` — polls + auto-discovers via register_missing_tabs; switches active to newest |

**Condition wait actions** (block until state is reached — no manual sleep polling):

| Action | Required args | Returns |
|--------|---------------|---------|
| `wait_for_url` | `target` (substring or `/regex/`), optional `timeout_ms` (default 10000) | `{matched, url, elapsed_ms}` — errors on timeout |
| `wait_for_ax_ready` | optional `min_nodes` (default 20), `timeout_ms` (default 10000) | `{node_count, elapsed_ms}` — polls AX tree every 200ms; errors on timeout |
| `wait_for_network_idle` | optional `idle_ms` (stable window, default 500), `timeout_ms` (default 15000) | `{elapsed_ms, request_count}` — stable when capture count unchanged for `idle_ms`; errors on timeout |

**Assertion actions** (return `{passed:true}` or MCP error on failure):

| Action | Required args | Returns |
|--------|---------------|---------|
| `assert_ax_contains` | `role` and/or `name` | `{passed, found, node}` — errors with details if no matching AX node |
| `assert_url_matches` | `target` (substring or `/regex/`) | `{passed, url}` — errors if current URL doesn't match |

**Multi-session actions** (named Chrome tabs):

| Action | Required args | Returns |
|--------|---------------|---------|
| `list_sessions` | — | `{sessions: [{session_id, tab_index, url}]}` |
| `close_session` | `target` (session_id) | `{status, closed_session_id}` — adjusts other sessions' indices |

**`session_id` param (optional on ALL browser_exec actions):** routes the
call to a named Chrome tab.  First use of a new `session_id` opens a fresh
tab.  Omitting `session_id` uses the default tab (index 0).

---

## Pre-Authorization (AuthZ)

Every `browser_exec` call passes through the AuthZ engine before execution.
By default Sirin ships in **permissive mode** — all calls pass through and an
audit entry is written.

### Modes

| Mode | Behaviour |
|------|-----------|
| `permissive` (default) | All calls allowed; audit log written |
| `selective` | Rules evaluated in order; unmatched calls allowed |
| `strict` | Rules evaluated in order; unmatched calls denied |

Configure in `config/authz.yaml` (created on first run if missing):

```yaml
mode: permissive          # permissive | selective | strict
audit_log: data/authz_audit.jsonl
rules:
  - url_glob: "https://internal.corp/*"
    action: deny
  - url_glob: "https://payments.stripe.com/*"
    action: ask
  - js_contains: "document.cookie"
    action: deny
```

### Decision pipeline (per `browser_exec` call)

1. Mode `permissive` → immediately allow (skip rules)
2. Rules evaluated in declaration order → first match wins
   - `allow` → proceed
   - `deny` → return error immediately
   - `ask` / `ask_with_learn` → emit notification to Live Monitor, wait ≤30s
3. No rule matched: `selective` → allow, `strict` → deny

**`ask` / `ask_with_learn` flow:** Sirin emits an authz-ask event visible in
the Monitor UI's yellow modal panel. The operator clicks **Allow** or **Deny**.
If no decision arrives within 30 seconds, the call is denied automatically.
`ask_with_learn` additionally saves the decision as a rule for future calls.

### Audit log rotation

`data/authz_audit.jsonl` auto-rotates at **10 MB**, keeping up to 5 backups
(`authz_audit.jsonl.1` … `.5`). No manual rotation needed.

---

## Live Monitor

The Monitor panel (`Sirin GUI → Monitor` tab) shows real-time browser activity
and gives the operator interactive control over running tests.

### Action Feed

Every browser_exec step emitted by a test or imperative call appears as a
timestamped event: `[tool] action → status`. The feed auto-scrolls to newest
entries while running.

### Screenshot Pane

Live JPEG thumbnail (500 ms interval, 80% quality) of the active Chrome
window. The **⏸ Pause stream** toggle freezes the thumbnail without stopping
the test.

### Control Bar

| Button | Effect on `browser_exec` calls |
|--------|-------------------------------|
| **Pause** | All future calls block at `gate()` until Resumed. In-flight call completes first. |
| **Step** | Unblocks exactly one queued call, then re-pauses automatically. |
| **Abort** | Signals abort — all subsequent `gate()` calls return an error, terminating the test run. |
| **Reset** | Clears abort flag and resumes. Use after inspecting an aborted session. |

Control state is shared between the GUI and MCP server via an atomic
`ControlState` singleton — these buttons work whether the test was triggered
from the GUI or from a `run_test_async` MCP call.

### Authz Modal

When an `ask` or `ask_with_learn` rule matches, a yellow panel appears in the
Monitor showing:

- Tool name and action
- The URL being requested
- **Allow** and **Deny** buttons

Clicking Allow/Deny resolves the 30s wait in the MCP server pipeline.

### Trace Replay

Load historical `.sirin/trace-*.ndjson` files to replay the action feed
offline. The replay dropdown shows available trace files newest-first with
human-readable timestamps derived from the filename. Screenshot pane is
disabled during replay.

---

## Workflow Patterns

### Pattern A — Test a known YAML goal
```
list_tests → run_test_async → poll get_test_result → done
```

### Pattern B — Test an ad-hoc URL
```
run_adhoc_test → poll get_test_result → if failed: get_screenshot
```

### Pattern C — Debug a failed run
```
get_test_result       → get error + analysis
get_screenshot        → see what the page looked like
get_full_observation  → un-truncated tool output per step
list_recent_runs      → is this test historically flaky?
list_fixes            → is an auto-fix already in progress?
```

### Pattern D — Diagnose Sirin itself
```
config_diagnostics → errors/warnings + structured text_report
```

### Pattern E — Imperative exploration
```
browser_exec(goto)       → navigate
browser_exec(console)    → JS errors
browser_exec(eval)       → inspect DOM
browser_exec(screenshot) → visual state
```

### Pattern F — Exact-string assertion (K14/K15) via accessibility tree

When asserting on numeric values, error messages, or specific copy
where vision LLM precision loss matters:

```
1. browser_exec({action:"goto", target:"https://app/wallet",
                 browser_headless:false})    # Flutter needs visible
2. browser_exec({action:"enable_a11y"})       # wake Flutter semantics
3. browser_exec({action:"ax_find", role:"text", name:"Total Assets"})
   → {found:true, nodes:[{backend_id: 142, ...}]}
4. browser_exec({action:"ax_value", backend_id: 142})
   → {text: "$7376.80"}                       # LITERAL, not "about 7377"
5. (perform an action)
6. browser_exec({action:"ax_value", backend_id: 142})
   → {text: "$7277.50"}
7. assert: 7376.80 - 7277.50 == 99.30          # exact diff
```

`ax_*` is faster than `screenshot_analyze` (no LLM call) and works on
Flutter CanvasKit (which has no real DOM but does expose semantics).

### Pattern G — Race-free network assertion (request body)

When the assertion is on what the client **sent**, not just received:

```
1. browser_exec({action:"click", target:"#transfer-btn"})  # fires POST
2. browser_exec({action:"wait_request",
                 target:"/api/wallet/transfer", timeout:5000})
   → {request:{url, method:"POST", status:200,
               req_body:'{"amount":"99.30",...}',  ← LITERAL
               body:'{"new_balance":"7277.50"}'}}
3. assert: parse req_body, amount === "99.30"
```

`wait_request` auto-installs the network capture, so no need to
prime with `install_capture` first.

### Pattern H — Test isolation between sequential runs

K-series tests run back-to-back share Chrome state by default
(K13's auth leaks into K14):

```
clear_state → goto → fresh login form, regardless of previous test
```

Doesn't restart Chrome. Wipes cookies (all domains), localStorage,
sessionStorage, IndexedDB, and Cache Storage.

### Pattern I — OAuth / popup tab handling

```
1. click → opens popup tab (Telegram OAuth, Google login, Stripe)
2. wait_new_tab → Sirin auto-discovers via register_missing_tabs
                  and switches `active` to the new tab
3. interact in popup
4. switch_tab(0) → back to original
```

Without `wait_new_tab`, the popup is invisible to Sirin (headless_chrome
doesn't auto-track tabs spawned by `window.open`).

### Pattern J — Quick page orientation with `page_state`

When you need situational awareness before deciding what to assert:

```
1. browser_exec({action:"goto", target:"https://app/dashboard"})
2. page_state()
   → {url:"https://app/dashboard", title:"Dashboard",
      ax_summary:"button:Logout, text:Balance $7376.80, ...",
      console_recent:[], screenshot_b64:"..."}
3. Read ax_summary to plan ax_find targets — no extra round-trips
```

Use `page_state` to orient at the start of an ad-hoc exploration or when
a paused test hands control back for inspection.

### Pattern K — Before/after diff with `ax_snapshot` + `ax_diff`

When an action should change specific UI elements and you want a
machine-readable delta rather than re-reading every value manually:

```
1. browser_exec({action:"ax_snapshot", id:"before_transfer"})
   → {snapshot_id:"before_transfer", count:84}

2. browser_exec({action:"click", target:"#transfer-btn"})
3. browser_exec({action:"wait_request", target:"/api/wallet/transfer"})

4. browser_exec({action:"ax_snapshot", id:"after_transfer"})
   → {snapshot_id:"after_transfer", count:84}

5. browser_exec({action:"ax_diff",
                 before_id:"before_transfer", after_id:"after_transfer"})
   → {added:[], removed:[],
      changed:[{node_id:"142", before_name:"$7376.80",
                              after_name:"$7277.50"}]}

6. assert changed[0].after_name == "$7277.50"
```

Faster than scanning the full `ax_tree` — only the delta is returned.

### Pattern K2 — Wait for async UI update with `wait_for_ax_change`

For async UIs where the change fires milliseconds after the action:

```
1. browser_exec({action:"ax_snapshot", id:"baseline"})
2. browser_exec({action:"click", target:"#submit"})
3. browser_exec({action:"wait_for_ax_change",
                 baseline_id:"baseline", timeout_ms:5000})
   → {changed:true,
      diff:{added:[], removed:[],
            changed:[{node_id:"22", before_name:"Pending",
                                   after_name:"Confirmed"}]}}
```

### Pattern M — Condition waits (no sleep polling)

Instead of inserting fixed-delay `wait` calls or polling externally:

```
1. browser_exec({action:"goto", target:"https://app/login"})
2. browser_exec({action:"click", target:"#login-btn"})
3. browser_exec({action:"wait_for_url",
                 target:"#/dashboard", timeout_ms:8000})
   → {matched:true, url:"https://app/#/dashboard", elapsed_ms:1240}
4. browser_exec({action:"wait_for_ax_ready",
                 min_nodes:20, timeout_ms:8000})
   → {node_count:47, elapsed_ms:600}
5. browser_exec({action:"assert_ax_contains", role:"text", name:"Welcome"})
   → {passed:true, found:true, node:{...}}
```

Wait for async data load to finish before asserting:
```
browser_exec({action:"wait_for_network_idle", idle_ms:800, timeout_ms:15000})
→ {elapsed_ms:2100, request_count:12}
```

### Pattern N — Multi-session (parallel Chrome tabs)

For tests that need two users, two contexts, or two tabs running together:

```
1. browser_exec({action:"goto",
                 target:"https://app/buyer",   session_id:"buyer"})
2. browser_exec({action:"goto",
                 target:"https://app/seller",  session_id:"seller"})
3. browser_exec({action:"ax_find", role:"button", name:"Buy",
                 session_id:"buyer"})
4. browser_exec({action:"ax_find", role:"button", name:"Sell",
                 session_id:"seller"})
5. browser_exec({action:"list_sessions"})
   → {sessions:[{session_id:"buyer",  tab_index:1, url:"..."},
                {session_id:"seller", tab_index:2, url:"..."}]}
6. browser_exec({action:"close_session", target:"buyer"})
```

Omitting `session_id` always targets the default tab (index 0).

### Pattern L — Fixture setup/cleanup

When a test needs the app in a specific state before the goal runs:

```json
run_adhoc_test({
  "url": "https://app.com/transfer",
  "goal": "Transfer $99.30 and verify balance decreases",
  "success_criteria": ["Balance shows $7277.50 after transfer"],
  "fixture": {
    "setup": [
      {"action":"goto",  "target":"https://app.com/login"},
      {"action":"click", "target":"#quick-login-test"},
      {"action":"wait",  "target":".dashboard"}
    ],
    "cleanup": [
      {"action":"clear_state"}
    ]
  }
})
```

Setup failure → test status `error`. Cleanup always runs (errors logged,
not surfaced). YAML test goals use the same `fixture:` key — see
`docs/test-runner-roadmap.md` for the full schema.

---

## Failure Classification (auto-fix)

When `auto_fix: true` and a test fails, triage runs an LLM classifier against
the failure context:

| Category | Auto-fix target | Meaning |
|----------|:---:|---------|
| `ui_bug` | → frontend repo | Visible rendering/interaction issue |
| `api_bug` | → backend repo | Network 4xx/5xx or bad response body |
| `flaky` | no spawn | <70% historical pass rate |
| `env` | no spawn | Browser/network infrastructure issue |
| `obsolete` | no spawn | Selector not found — test needs update |

Dedup rules (see `list_fixes` outcome values above):
- Any `pending` fix within 30 minutes for the same test → skipped
- Last 3 consecutive `failed` outcomes → skipped (circuit breaker)

---

## Common Gotchas

### Flutter / WebGL target? Set `browser_headless: false`
CanvasKit and WebGL content do not paint reliably in headless Chrome.
Symptom: screenshots (and therefore `screenshot_analyze`) return an
all-black PNG regardless of what the page should show. Fix: pass
`browser_headless: false` to `run_adhoc_test` / `browser_exec`, or set
`browser_headless: false` in the test YAML, or set the global env
`SIRIN_BROWSER_HEADLESS=false` before launching Sirin.

### Hash-only routes (e.g. `/#/admin/users`)
Fragment changes don't emit `Page.frameNavigated`. Sirin auto-detects
this case and uses `location.hash = ...` + short settle delay instead
of waiting for a navigation event. No user action needed — just works.

### Vision approximates numbers — use `ax_*` for exact values
`screenshot_analyze` describes a page ("balance is about 7377 USDT");
the accessibility tree returns the literal string ("$7376.80").  For
any test that compares amounts, IDs, error messages, or hash strings
exactly, use the `ax_*` actions instead.

### Flutter semantics tree collapses
Flutter Web auto-disables its semantics tree when no AT activity is
detected.  Symptom: `ax_tree` returns 1-2 nodes instead of dozens.

Two distinct situations look identical: (1) cold start — needs bootstrap;
(2) post-navigation teardown — Flutter rebuilding, tree self-recovers in ~1s.

Sirin's `get_full_tree` detects ≤2 nodes, **polls 3×400ms first** to allow
self-recovery, then calls `enable_a11y` (placeholder click only — **Tab×2
permanently removed** in Issue #20 because it triggered URL resets on
hash-route Flutter apps via keyboard event delivery).

Use `wait_for_ax_ready` after navigation to block until the tree is ready:
```json
{"action":"wait_for_ax_ready","min_nodes":20,"timeout_ms":8000}
```

### Sequential tests share state — call `clear_state` between them
Cookies, localStorage, sessionStorage, IndexedDB all persist across
test runs (Chrome stays alive between them, by design — speed).
If K13 logs in and K14 expects a logged-out home page, K14 will
fail unexpectedly.  Insert `browser_exec({action:"clear_state"})`
between tests, or as the first step of each test goal.

### `network` returns nothing right after a click — race
The fetch/XHR fires asynchronously; calling `network` immediately
after a click usually misses it.  Use `wait_request` instead — it
polls the capture array until a matching URL substring appears.

### `switch_tab(1)` "out of range" after OAuth click
headless_chrome doesn't auto-track tabs spawned by `window.open` /
`target="_blank"`.  Use `wait_new_tab` after the click — it calls
`register_missing_tabs` to force discovery and adopts the popup
into the singleton.

### `ax_type` typed but value didn't change
Flutter Canvas inputs sometimes drop characters; React controlled
inputs may transform values; masked inputs add formatting.  Use
`ax_type_verified` to read back the actual value and check
`matched` (substring of typed text) before asserting.

### Mode-switch race
Switching between `browser_headless: true` and `browser_headless: false`
between calls triggers a fresh Chrome launch. The first `navigate()`
after launch might hit "wait: The event waited for never came" because
CDP subscriptions haven't fully initialised. Sirin handles this with
a 600ms settle delay + 1 auto-retry — transparent to the caller but
shows up in server logs.

### Port 7700 stuck
If Sirin was killed abruptly on Windows, the port can linger in
TIME_WAIT / CLOSE_WAIT for ~2 minutes. Sirin auto-retries bind 3× with
2s backoff; if still failing, launch with `SIRIN_RPC_PORT=7701`.

### `ax_find` with `name_regex` — anchoring
`name_regex` is matched via Rust's `regex` crate (no implicit anchoring).
`"^Balance$"` matches exactly; `"Balance"` matches substring. Combine
with `not_name_matches` to exclude irrelevant nodes:
```json
{"action":"ax_find","role":"text","name_regex":"\\$[0-9]",
 "not_name_matches":["Total","Header"],"limit":5}
```

### `ax_snapshot` IDs are session-scoped
Snapshot IDs live in Sirin's in-process memory — they do not persist
across Sirin restarts. Take a fresh snapshot at the start of each test
session; don't store IDs across days.

### AuthZ Ask timeout
When an `ask` rule matches, Sirin waits up to 30 seconds for an operator
decision from the Monitor UI. If the Monitor tab is not open, the decision
never arrives and the call is denied. Either switch to `permissive` mode or
keep the Monitor visible when running guarded tests.

---

## Safety Guarantees

- Ad-hoc runs persist to SQLite but skip auto-fix verification (no YAML to re-run)
- Verification re-runs always use `auto_fix=false` to prevent recursion
- Browser singleton auto-recovers from dead CDP connections (health check
  + one-shot retry in `with_tab`)
- LLM parse errors trigger reprompt up to `retry_on_parse_error` times (YAML
  per-test, default 3) before aborting
- Observation truncation at 800 chars includes a hint pointing to
  `get_full_observation` for the full text
- AuthZ audit log auto-rotates at 10 MB (5 backups kept)
- AuthZ mode defaults to `permissive` — no accidental denial on fresh installs

---

## Related

- `.claude/skills/sirin-launch/SKILL.md` — lifecycle (start/stop/restart)
- `.claude/skills/sirin-test/SKILL.md` — test workflows incl. Flutter playbook
- `docs/test-runner-roadmap.md` — feature evolution history
