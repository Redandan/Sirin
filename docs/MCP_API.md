# Sirin MCP API Reference

`http://127.0.0.1:7700/mcp` — MCP 2024-11-05 Streamable HTTP (JSON-RPC 2.0 over POST)

Port override: `SIRIN_RPC_PORT=<n>` env var at Sirin launch time (default `7700`).
When port 7700 is held by a zombie socket from a previously-killed Sirin, set
this to `7701` or similar.

Use this when Sirin is running and you want to drive it from an external agent
(Claude Code, Claude Desktop, custom scripts).

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

### Test Runner (10 tools)

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
  "browser_headless":false
}}
```
**`browser_headless` (optional):** `false` required for Flutter CanvasKit /
WebGL targets (they won't paint in headless Chrome → screenshots come back
black). Default reads `SIRIN_BROWSER_HEADLESS` env (itself defaulting to
`true`).

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

## Safety Guarantees

- Ad-hoc runs persist to SQLite but skip auto-fix verification (no YAML to re-run)
- Verification re-runs always use `auto_fix=false` to prevent recursion
- Browser singleton auto-recovers from dead CDP connections (health check
  + one-shot retry in `with_tab`)
- LLM parse errors trigger reprompt up to `retry_on_parse_error` times (YAML
  per-test, default 3) before aborting
- Observation truncation at 800 chars includes a hint pointing to
  `get_full_observation` for the full text

## Related

- `.claude/skills/sirin-launch/SKILL.md` — lifecycle (start/stop/restart)
- `.claude/skills/sirin-test/SKILL.md` — test workflows incl. Flutter playbook
- `docs/test-runner-roadmap.md` — feature evolution history
