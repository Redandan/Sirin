---
name: sirin-test
description: This skill should be used when the user asks to "test a website", "run an E2E test", "verify a user flow", "check if a page works", "run browser tests", or mentions Sirin's testing capabilities. Provides the workflow for driving Sirin's AI-powered browser test runner via its MCP API (:7700/mcp).
version: 1.4.0
---

# Sirin Browser Test Runner

Drive Sirin's AI-powered browser testing from external Claude Code sessions. Unlike Puppeteer/Playwright (scripted), Sirin tests are **goal-driven** — you describe what should happen, the LLM inside Sirin figures out how to click/type/verify.

## When This Skill Applies

- User asks to test a web application ("test the checkout flow", "verify login works")
- User wants E2E / smoke / regression testing on a specific URL
- User asks about Sirin's test runner capabilities
- User wants automated failure diagnosis + bug fixing loop

## Prerequisites

1. **Sirin is running** with MCP server on `http://127.0.0.1:7700/mcp`
   - Check: `curl -X POST http://127.0.0.1:7700/mcp -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'`
   - If not running, apply the **`sirin-launch`** skill first — it has
     a full status-check → build → detached launch → readiness-poll
     workflow. Do not tell the user "please start Sirin" without trying
     to launch it yourself.

2. **Sirin is registered as MCP server** in the caller's Claude Desktop config (or equivalent):
   ```json
   {
     "mcpServers": {
       "sirin": { "url": "http://127.0.0.1:7700/mcp" }
     }
   }
   ```

3. **Chrome** installed for headless browser control
4. **Test goals** defined in `config/tests/*.yaml` inside the Sirin repo

## Available MCP Tools

| Tool | Purpose | When to use |
|------|---------|------------|
| `list_tests` | Enumerate YAML tests in `config/tests/` | "What tests exist?" |
| `run_test_async` | Run a YAML-defined test | When matching test_id exists |
| `run_adhoc_test` | Test a URL with inline goal, no YAML | **User wants to test arbitrary URL** |
| `persist_adhoc_run` | Promote a passing ad-hoc run → permanent YAML test | **User says "save this as a regression test"** after a successful ad-hoc explore |
| `get_test_result` | Poll run status by `run_id` | Every 3-5s while test runs |
| `get_screenshot` | Fetch base64 PNG of failure | status=failed/timeout/error |
| `get_full_observation` | Un-truncated tool output | Observation mentions `[truncated: ...]` |
| `list_recent_runs` | Historical test executions (SQLite) | Debug flakiness / see patterns |
| `list_fixes` | Auto-fix history (claude_session spawns) | Check if a fix is in-flight |
| `config_diagnostics` | LLM/router/vision health report | Tests failing mysteriously — self-diagnose |
| `diagnose` | Sirin self-diagnostic snapshot (version/build/host/Chrome/LLM/recent errors + pre-filled GitHub issue body) | **You hit a bug in Sirin itself — call this BEFORE bothering the user.** Use the snapshot to decide retry / suggest upgrade / file issue with `report_issue_template.body` |
| `page_state` | URL + title + AX summary + console + screenshot | **Quick orientation before deeper inspection** |
| `browser_exec` | Single imperative browser action | **Debug / one-off exploration without a goal** |

`browser_exec` accepts these `action` values:

**Standard:** `goto`, `screenshot`, `screenshot_analyze`, `click`, `click_point`, `type`, `read`, `eval`, `wait`, `exists`, `attr`, `scroll`, `key`, `console`, `network`, `url`, `title`, `close`, `set_viewport`

**Accessibility tree** (literal string, no vision approximation — **use these for K14/K15 exact-value assertions**):
- `enable_a11y` — trigger Flutter semantics bridge (call before `ax_tree` on Flutter Canvas apps)
- `ax_tree` — full a11y node list with `role`, literal `name`, literal `value`, `backend_id`, `child_ids`
- `ax_find` (params: `role`, `name` substring, `name_regex` full Rust regex, `not_name_matches` exclusion array, `limit` int default 1) — returns `{found, count, nodes:[...]}` array; single-match compat: check `nodes[0]`
- `ax_snapshot` (param: `id` optional string) — captures current AX tree to memory; returns `{snapshot_id, count}`
- `ax_diff` (params: `before_id`, `after_id`) — compares two snapshots; returns `{added:[...], removed:[...], changed:[{node_id, before_name, after_name}]}`
- `wait_for_ax_change` (params: `baseline_id`, `timeout_ms` default 5000) — blocks until tree differs; returns `{changed:true, diff:{...}}` or timeout error
- `ax_value` (param: `backend_id`) — exact text content (`value || name`)
- `ax_click` / `ax_focus` (param: `backend_id`) — interaction by DOM backend id
- `ax_type` (params: `backend_id`, `text`) — focus + insertText
- `ax_type_verified` (same params) — types then reads back, returns `{typed, actual, matched}` so you know if Flutter dropped chars or the input formatted the value

**Robustness** (test isolation, race-free assertions, popups):
- `clear_state` — wipe cookies + localStorage + sessionStorage + IndexedDB + caches between tests so K13 can't leak auth into K14
- `wait_request` (params: `target` URL substring, `timeout` ms default 10000) — block until a fetch/XHR matching the substring is captured; auto-installs network capture; eliminates click-then-read races; returns the entry **including `req_body`**
- `wait_new_tab` (param: `timeout` ms default 10000) — block until a popup/OAuth tab opens; auto-discovers via `register_missing_tabs` and switches `active` to the new tab

**Condition waits** (block until state is reached — no manual sleep polling):
- `wait_for_url` (params: `target` substring or `/regex/`, `timeout_ms` default 10000) — block until URL matches; `{matched, url, elapsed_ms}`; errors on timeout
- `wait_for_ax_ready` (params: `min_nodes` default 20, `timeout_ms` default 10000) — block until AX tree has ≥ min_nodes nodes; use after `enable_a11y` on Flutter apps; `{node_count, elapsed_ms}`
- `wait_for_network_idle` (params: `idle_ms` stable window default 500, `timeout_ms` default 15000) — block until network requests stop firing; `{elapsed_ms, request_count}`

**Assertion shortcuts** (error on failure — no extra if-check needed in scripts):
- `assert_ax_contains` (params: `role`, `name`) — errors with helpful message if no matching AX node found; `{passed, found, node}`
- `assert_url_matches` (params: `target` substring or `/regex/`) — errors if current URL doesn't match; `{passed, url}`

**Multi-session** (named Chrome tabs — for parallel or multi-user tests):
- `list_sessions` — `{sessions: [{session_id, tab_index, url}]}`
- `close_session` (param: `target` session_id) — close named tab

Every `browser_exec` action accepts optional **`session_id`** param to route to a named tab. First use opens a new tab; subsequent uses reuse it.

**`ax_find` scroll params** (for long lists / virtualized containers):
- `scroll: true` — if not found initially, scroll down `scroll_step_px` px (default 400) and retry
- `scroll_max: N` (default 10) — max number of scrolls before giving up; returns `{found, node, scrolled_times}`

Plus Sirin's normal MCP tools: `memory_search`, `skill_list`, `teams_pending`, `teams_approve`, `trigger_research`.

## Core Workflows

### Workflow A — Run a single test with polling

```
1. list_tests(tag="smoke")
   → { count: 3, tests: [{id: "wiki_smoke", ...}, ...] }

2. run_test_async(test_id="wiki_smoke", auto_fix=false)
   → { run_id: "run_20260416_143022_123", status: "queued" }

3. Loop every 3s:
   get_test_result(run_id="run_20260416_...")
   → { status: "running", details: { step: 4, current_action: "click" }}
   ...
   → { status: "passed", details: { iterations: 6, duration_ms: 12000 }}

4. If status != passed, fetch failure artifacts:
   get_screenshot(run_id=...) → base64 PNG
```

### Workflow B — Run test with auto-fix

```
run_test_async(test_id="checkout_flow", auto_fix=true)

# auto_fix=true makes Sirin spawn a Claude Code session in the
# frontend/backend repo when the failure is classified as
# ui_bug or api_bug. Fire-and-forget; check repo for commits.
```

### Workflow B.5 — Ad-hoc URL test (NO YAML needed)

When the user names an arbitrary URL that isn't in `config/tests/`:

```
User: "Test if https://example.com/signup still works with email
       'test@test.com' and verify we land on /welcome"

1. run_adhoc_test({
     url: "https://example.com/signup",
     goal: "Register with email test@test.com and reach /welcome",
     success_criteria: ["URL ends with /welcome", "No console errors"],
     // Flutter / WebGL targets? add:
     // browser_headless: false
   })
   → { run_id: "run_...", status: "queued" }

2. Poll with get_test_result as in Workflow A.
```

This is the right answer when `list_tests` doesn't show a matching
existing test.  Don't refuse the user with "no such test".

**Flutter/WebGL ad-hoc:** pass `browser_headless: false`. Same reason
as the Flutter playbook section below — CanvasKit won't paint in
headless Chrome.

### Workflow B.5.1 — Promote ad-hoc → permanent regression test

Once an ad-hoc explore passes, save it so the next regression cycle
re-runs it automatically:

```
1. run_adhoc_test({...}) → run_id
2. Poll get_test_result until status == "passed"
3. persist_adhoc_run({
     run_id,
     test_id: "login_flow",        // [a-z0-9_]+, NOT starting with adhoc_
     name:    "Login flow regression",  // optional override
     tags:    ["smoke","auth"],     // optional override
     bump_iterations: true,         // default; max(used+5, original)
     overwrite: false               // default; refuses if file exists
   })
   → { test_id, yaml_path, iterations_used, criteria_count, tags }
```

Now `run_test_async({test_id:"login_flow"})` re-runs the same goal as
regression. **Do not** call `persist_adhoc_run` for failed runs — it
refuses (would write a YAML that always fails).

The persisted YAML carries over `goal`, `success_criteria`, `locale`,
`url_query`, `browser_headless`, `fixture`, `timeout_secs`, and
`retry_on_parse_error` from the original ad-hoc run. The `adhoc` tag
is stripped and replaced with `adhoc-derived` so `list_tests` can
distinguish persisted runs from in-flight ones.

**When to suggest this proactively:** if the user says
"that worked, save it" / "make it a regression" / "next time too" /
"let's add this to the suite" right after a successful ad-hoc run.

### Workflow B.6 — Imperative browser debug

For manual exploration without a goal, use `browser_exec` directly:

```
browser_exec({action: "goto",       target: "https://site.com"})
browser_exec({action: "screenshot"}) → base64 PNG
browser_exec({action: "screenshot_analyze",
              target: "Describe what's on screen"}) → Gemini Vision text
browser_exec({action: "console"})   → JS errors
browser_exec({action: "network"})   → fetch/XHR log
browser_exec({action: "click",      target: "#btn"})
browser_exec({action: "read",       target: "h1"})
```

Useful for:
- Diagnosing why a test fails before retrying
- Answering "what's on that page right now?"
- Step-by-step exploration when the user is iterating on a flow

### Workflow B.7 — Exact-string assertions (K14/K15) via accessibility tree

When the user needs **exact** text comparison — wallet balances, error
messages, token counts, transaction hashes — vision LLMs lose precision
("about $7377" vs the real "$7376.80"). Use the `ax_*` actions instead:

```
1. browser_exec({action: "goto", target: "https://app.com/wallet",
                 browser_headless: false})  # Flutter needs visible Chrome

2. browser_exec({action: "enable_a11y"})    # required once for Flutter
                                              # Canvas apps to expose
                                              # the semantics tree

3. browser_exec({action: "ax_find", role: "text", name: "Total Assets"})
   → { found: true, nodes: [{ backend_id: 142, name: "Total Assets",
                              role: "StaticText", ... }] }

4. browser_exec({action: "ax_value", backend_id: 142})
   → { backend_id: 142, text: "$7376.80" }    ← LITERAL string

5. (do something that changes the balance)

6. browser_exec({action: "ax_value", backend_id: 142})
   → { backend_id: 142, text: "$7277.50" }    ← LITERAL string

7. assert: 7376.80 - 7277.50 == 99.30  ← exact, not "about"
```

**Tips:**
- For Flutter apps, `enable_a11y` is required to wake the semantics
  tree. After calling it, use `wait_for_ax_ready` to block until the
  tree fully populates (Flutter can take up to ~1s to build):
  ```
  browser_exec({action: "wait_for_ax_ready", min_nodes: 20, timeout_ms: 8000})
  ```
  Sirin also auto-retries the bootstrap internally if `ax_tree`
  returns ≤2 nodes (polls 3×400ms for self-recovery first, then
  re-triggers `enable_a11y` once).
- `ax_find` `name` is **substring + case-insensitive**. Pass exact
  text to `ax_value` for the precise comparison.
- `ax_click` uses `DOM.getBoxModel` → element centre point. More
  reliable than CSS selectors on Flutter Canvas / shadow DOM.
- For text input on Flutter, use `ax_type` (focus + insertText)
  rather than `type` (CSS-selector-based, won't find Canvas inputs).

### Workflow B.8 — Race-free network assertion (request body)

When the assertion is on what was **sent** (not just received) — wallet
transfer amount, OAuth callback params, form field values posted to the
backend — use `wait_request` so you don't read `network` before the
request fires:

```
1. browser_exec({action: "click", target: "#transfer-btn"})  # fires POST
2. browser_exec({action: "wait_request",
                 target: "/api/wallet/transfer",
                 timeout: 5000})
   → { request: {
         url: "https://api.example.com/api/wallet/transfer",
         method: "POST",
         status: 200,
         req_body: '{"amount":"99.30","to":"0xabc..."}',  ← LITERAL
         body:     '{"new_balance":"7277.50",...}',
         ts: 1729...
     }}
3. assert: request.req_body parses as JSON with amount="99.30"
```

vs the broken pattern (race):
```
✗ click → immediately call `network` → no entry yet → assertion fails
```

`wait_request` auto-installs the network capture, so no need to call
`install_capture` first.

### Workflow B.9 — Test isolation between sequential tests

When running K-series tests back-to-back, the previous test's auth
session, cookies, and localStorage will bleed into the next:

```
# Before each test (or at the start of each YAML test goal):
1. browser_exec({action: "clear_state"})
   → wipes cookies + localStorage + sessionStorage + IndexedDB + caches
2. browser_exec({action: "goto", target: "https://app/"})
   → fresh session; login form appears even if previous test was logged in
```

`clear_state` does NOT close Chrome — same process, same tab, just
zeroed state. Much faster than a full browser restart.

### Workflow B.10 — OAuth / popup tab handling

When clicking a button opens a new tab (Telegram OAuth, Google login,
Stripe checkout):

```
1. browser_exec({action: "click", target: "#login-with-google"})
2. browser_exec({action: "wait_new_tab", timeout: 5000})
   → { status: "new tab opened", active_tab: 1 }
   # Sirin auto-discovers the popup and switches active to it
3. browser_exec({action: "url"})  → google.com/oauth/...
4. (interact with the OAuth tab via ax_* / click)
5. browser_exec({action: "switch_tab", index: 0})  # back to original
6. browser_exec({action: "ax_value", backend_id: 142})  # read post-login state
```

Without `wait_new_tab`, the popup tab is invisible to Sirin and
`switch_tab(1)` would fail with "out of range".

### Workflow B.11 — Quick orientation with `page_state`

When you need situational awareness before deciding what to assert or
which `ax_find` selectors to use:

```
1. browser_exec({action: "goto", target: "https://app/dashboard"})
2. page_state()
   → { url: "https://app/dashboard",
       title: "Dashboard",
       ax_summary: "button:Logout, text:Balance $7376.80, button:Transfer ...",
       console_recent: [],
       screenshot_b64: "..." }
3. Read ax_summary → plan which ax_find names to use without extra calls
```

`page_state` bundles 4 calls (url + title + condensed ax_tree + console)
into one. Use it at the start of any exploratory session.

### Workflow B.12 — Before/after diff with `ax_snapshot` + `ax_diff`

When you want a machine-readable delta rather than manually comparing
two full ax_tree dumps:

```
# Before action
1. browser_exec({action: "ax_snapshot", id: "before_transfer"})
   → { snapshot_id: "before_transfer", count: 84 }

# Trigger the change
2. browser_exec({action: "click", target: "#transfer-btn"})
3. browser_exec({action: "wait_request", target: "/api/wallet/transfer"})

# After action
4. browser_exec({action: "ax_snapshot", id: "after_transfer"})

# Get diff
5. browser_exec({action: "ax_diff",
                 before_id: "before_transfer",
                 after_id:  "after_transfer"})
   → { added: [], removed: [],
       changed: [{ node_id: "142",
                   before_name: "$7376.80",
                   after_name:  "$7277.50" }] }

6. assert changed[0].after_name == "$7277.50"
```

**For async UIs** (change fires a few ms after the click), use
`wait_for_ax_change` instead of snapshotting twice:

```
1. browser_exec({action: "ax_snapshot", id: "baseline"})
2. browser_exec({action: "click", target: "#submit"})
3. browser_exec({action: "wait_for_ax_change",
                 baseline_id: "baseline", timeout_ms: 5000})
   → { changed: true,
       diff: { changed: [{ node_id: "22",
                           before_name: "Pending",
                           after_name:  "Confirmed" }] } }
```

### Workflow B.13 — Fixture setup/cleanup

When the test needs the app in a specific state before the goal runs
(e.g. logged in, specific data loaded):

```
run_adhoc_test({
  url:  "https://app.com/transfer",
  goal: "Transfer $99.30 and verify the balance decreases",
  success_criteria: ["Balance shows $7277.50 after transfer"],
  fixture: {
    setup: [
      {action: "goto",  target: "https://app.com/login"},
      {action: "click", target: "#quick-login-test"},
      {action: "wait",  target: ".dashboard", timeout_ms: 5000}
    ],
    cleanup: [
      {action: "clear_state"}
    ]
  }
})
```

- `setup` runs before the ReAct loop; failure → test aborts with `error` status
- `cleanup` runs unconditionally after the loop (even on timeout/failure);
  cleanup errors are logged and ignored

The same `fixture:` key works in YAML test goals — see Test YAML Structure.

### Workflow B.14 — Condition waits (no sleep polling)

Instead of inserting fixed-delay `wait` calls or polling externally,
use condition waits to block until a specific state is reached:

```
# Wait for redirect URL (after login, after form submit):
browser_exec({action: "wait_for_url", target: "#/dashboard", timeout_ms: 8000})
→ { matched: true, url: "https://app/#/dashboard", elapsed_ms: 1240 }

# Wait for Flutter AX tree after enable_a11y:
browser_exec({action: "wait_for_ax_ready", min_nodes: 20, timeout_ms: 8000})
→ { node_count: 47, elapsed_ms: 600 }

# Wait for async data load to settle before asserting:
browser_exec({action: "wait_for_network_idle", idle_ms: 800, timeout_ms: 15000})
→ { elapsed_ms: 2100, request_count: 12 }

# Inline assertion (errors if not found — cleaner than if/else):
browser_exec({action: "assert_ax_contains", role: "text", name: "Welcome"})
→ { passed: true, found: true, node: {...} }
browser_exec({action: "assert_url_matches", target: "#/dashboard"})
→ { passed: true, url: "https://app/#/dashboard" }
```

### Workflow B.14.5 — When Sirin itself misbehaves: `diagnose` first

**Two-tier diagnostic protocol.** When you (Tier 1, external Claude session)
hit a bug *in Sirin* — `browser_exec` returns nonsense, MCP errors out, a
test produces obviously-wrong output — do this BEFORE bothering the user:

```
1. diagnose() → snapshot
   {
     identity:   { version, git_commit, build_date, uptime_secs, ... },
     chrome:     { running, version, headless, tab_count, ... },
     llm:        { provider, model, vision_capable_hint },
     update:     { state, current, latest, release_notes_url },
     recent_errors: ["...20 most recent ERROR/WARN log lines..."],
     report_issue_template: { title_hint, body, github_url }
   }
```

Decision matrix from the snapshot:

| Symptom in snapshot | Action |
|---|---|
| `update.state == "update_available"` and you're on an older version | Tell the user "you're on {current} but {latest} is out — please update" and link `release_notes_url`. Don't file an issue against the old version. |
| `uptime_secs < 30` and `recent_errors` shows startup race | Likely cold-start race — wait 5s and retry the original call. |
| `chrome.running == false` and you tried a `browser_exec` | Sirin's Chrome isn't bound — call `browser_exec(action: goto, target: ...)` first to launch it, then retry. |
| `llm.vision_capable_hint == false` and you needed `screenshot_analyze` | Wrong model for the job — tell the user to switch (e.g. `OLLAMA_MODEL=gemma3:12b`). |
| Nothing obvious — looks like a real bug | File an issue at `report_issue_template.github_url` with: title = `report_issue_template.title_hint` (replace `<one-line summary>` with your own), body = `report_issue_template.body` plus your **Reproduction** and **What you tried** sections (the env block is already filled in for you). |

Cost: ~5–20 ms. Safe to call on every error in your error-handling path.
Sirin doesn't cache; cost is dominated by one CDP round-trip to Chrome.

### Workflow B.15 — Multi-session (parallel Chrome tabs)

For multi-user tests or workflows that need two tabs simultaneously:

```
# Open two sessions (named Chrome tabs):
browser_exec({action: "goto", target: "https://app/buyer",  session_id: "buyer"})
browser_exec({action: "goto", target: "https://app/seller", session_id: "seller"})

# Interact in each session independently:
browser_exec({action: "ax_find", role: "button", name: "Buy",  session_id: "buyer"})
browser_exec({action: "ax_find", role: "button", name: "Sell", session_id: "seller"})

# List active sessions:
browser_exec({action: "list_sessions"})
→ { sessions: [
     { session_id: "buyer",  tab_index: 1, url: "https://app/buyer" },
     { session_id: "seller", tab_index: 2, url: "https://app/seller" }
  ]}

# Close a session when done:
browser_exec({action: "close_session", target: "buyer"})
```

Omitting `session_id` always targets the default tab (index 0).

### Workflow C — Debug a failed run

```
1. get_test_result(run_id=...) 
   → status=failed, details.error="could not find submit button"

2. get_screenshot(run_id=...) 
   → see what the page actually looked like

3. For each step where observation was truncated:
   get_full_observation(run_id=..., step=3)
   → full network/console log
```

## Test YAML Structure

Tests live at `config/tests/<id>.yaml`:

```yaml
id: checkout_happy_path             # unique id
name: "Happy-path checkout test"
url: "https://shop.example.com/cart"
goal: |
  User with items already in cart should be able to click checkout,
  fill shipping info, choose credit card, and see order confirmation.

max_iterations: 15                   # default 15 (raise for complex flows)
timeout_secs: 120                    # default 120s
retry_on_parse_error: 3              # default 3 (LLM JSON parse retries)
locale: en                           # zh-TW (default) / en / zh-CN

# Flutter / WebGL / Canvas-rendered apps: set to false or vision won't work
# (CanvasKit doesn't paint in headless Chrome → screenshots come back black)
browser_headless: true               # default reads SIRIN_BROWSER_HEADLESS env

# Optional URL query merge (e.g. force Flutter HTML renderer if app supports it)
url_query:
  # flutter-web-renderer: html       # uncomment for Flutter apps that honor it

success_criteria:                    # LLM judges these at the end
  - "URL contains /order-confirmation"
  - "Page shows order number starting with ORD-"
  - "No console errors during checkout"

tags: [smoke, checkout, critical]    # for filter via list_tests(tag=...)

# Optional fixture: setup/cleanup around the ReAct loop
# Setup failure → status=error; cleanup always runs
fixture:
  setup:
    - {action: "goto",  target: "https://shop.example.com/login"}
    - {action: "click", target: "#test-login"}
    - {action: "wait",  target: ".cart-ready", timeout_ms: 5000}
  cleanup:
    - {action: "clear_state"}
```

### Writing good goals

**Bad:** "Click the login button" (that's a step, not a goal)
**Good:** "User can log in with test credentials and reach dashboard"

Let the LLM figure out the steps. Only write steps if they're non-obvious or order-dependent.

## Common Patterns

### Flutter Web apps (CanvasKit) — **REQUIRED: browser_headless: false + vision**

Flutter apps built with CanvasKit (the default for production) have
**two separate traps**:

**Trap 1 — WebGL in headless = blank canvas.**
CanvasKit uses WebGL. Chrome's headless mode doesn't paint WebGL
content reliably → `get_screenshot` returns an all-black PNG regardless
of what the page should show. This ALSO defeats vision LLM — it can
only say "page is black".

**→ FIX: `browser_headless: false` in the test YAML.**

```yaml
id: flutter_smoke
url: "https://your-flutter-app.example.com/"
browser_headless: false   # ← REQUIRED for Flutter Canvas apps
goal: |
  ...
```

Or globally via `SIRIN_BROWSER_HEADLESS=false` env before launching Sirin.

Chrome will open a visible window. Flutter's WebGL then paints
normally. Screenshots are real content.

**Trap 2 — DOM is empty** (even with visible Chrome).
CanvasKit paints to `<canvas>`, not HTML elements. `read`, `exists`,
`eval(document.body.innerText)` all return empty/false. Don't use
those for text assertions.

**→ Two FIXes, by use case:**

**A) Exact text comparison (K14/K15) — use `ax_*` (preferred)**

The accessibility tree exposes Flutter's semantics nodes with **literal**
strings (`"$7376.80"`, not vision's "about $7377"). Required when the
test asserts numbers, error messages, or specific copy.

```yaml
goal: |
  ⚠️ Flutter CanvasKit app. Use accessibility tree for exact assertions:
    1. {action:"enable_a11y"}                # wake Flutter semantics
    2. {action:"ax_find", role:"text", name:"Total Assets"}
       → nodes[0].backend_id of the balance display
    3. {action:"ax_value", backend_id: <N>}  → literal "$7376.80"
  Compare strings exactly. Don't use screenshot_analyze for numbers.

success_criteria:
  - "ax_value of total assets equals $7376.80 before action"
  - "ax_value of total assets equals $7277.50 after action"
```

**B) Visual / layout / "is it broken" checks — use `screenshot_analyze`**

When the question is fuzzy ("does the login form look right?",
"is the page rendered at all?"), vision is fine.

```yaml
goal: |
  ⚠️ Flutter CanvasKit app. Use screenshot_analyze for visual checks:
    {action:"screenshot_analyze",
     target:"Does the page show the login form with an email field?"}

success_criteria:
  - "Vision confirms the brand title is visible"
  - "Vision confirms login form with email input exists"
  - "Vision confirms page has actual content (not blank/black)"
```

The last criterion defends against false positives where vision might
report "blank screen" when the screenshot really was blank (Trap 1
not yet applied).

**(Optional workaround) — `url_query` HTML renderer:**

```yaml
url_query:
  flutter-web-renderer: html
```
Only works if the app allows switching renderers. Many production
Flutter apps hardcode CanvasKit at build time (`<body flt-renderer=
"canvaskit">`) and ignore this query. Probe first:

```
browser_exec({action:"eval",
              target:"document.body.getAttribute('flt-renderer')"})
```

If it returns `"canvaskit"`, the app ignores `url_query` — use
`browser_headless: false` + vision instead.

**Interaction on CanvasKit:** combine vision (find element) with
coordinate clicks (click):
```yaml
{action:"click_point", x:380, y:330}
```

For Agora Market specifically: see `config/tests/agora_market_smoke.yaml`
— working end-to-end example using `browser_headless: false` + vision.

### Asynchronous UIs

```yaml
goal: |
  Navigate to the dashboard. Wait for the "Projects" list to appear
  (it loads via API, may take 2-3 seconds). Verify at least one
  project is visible.
```

The LLM will insert `wait` actions as needed.

### Known failure investigation

If a user says "this test flakes sometimes":

```
1. Run it 5 times: for i in 1..5: run_test_async(...) → collect run_ids
2. For each failed run: get_test_result(run_id) + get_screenshot
3. Compare failure patterns across runs
4. Sirin's triage will also auto-classify as flaky if historical
   success rate <70%
```

## Pre-Authorization (AuthZ) and Live Monitor

### AuthZ — what it does
Every `browser_exec` call passes through the AuthZ engine. Default mode is
**permissive** (all calls pass). In `selective` / `strict` mode, rules in
`config/authz.yaml` gate calls by URL glob, regex, or JS expression.

An `ask` rule will **block** the call up to 30s waiting for an operator
decision. If the Monitor tab is not open, the decision never arrives and the
call is denied. Keep the Monitor visible when testing with `ask` rules.

### Live Monitor — interactive control
The Monitor tab in Sirin's GUI shows:
- **Action feed** — every browser_exec step as it executes
- **Screenshot pane** — live 500ms JPEG thumbnail of Chrome
- **Control bar** — Pause / Step / Abort / Reset buttons
- **Authz modal** — yellow panel for pending Ask decisions

**Pause** blocks all future `browser_exec` calls until Resumed.
**Step** unblocks exactly one call then re-pauses — useful for single-stepping.
**Abort** terminates the test run (all subsequent calls error).

Control affects both GUI-launched and MCP-launched tests (shared atomic state).

## Failure Classification (Auto-Triage)

When a test fails, Sirin classifies it via LLM:

| Category | Auto-fix? | Meaning |
|----------|:---:|---------|
| `ui_bug` | → frontend repo | Visible rendering/interaction issue |
| `api_bug` | → backend repo | Network tab shows 4xx/5xx |
| `flaky` | no | <70% historical pass rate |
| `env` | no | Chrome/network infrastructure issue |
| `obsolete` | no | Selector not found — UI probably changed |

`auto_fix=true` spawns a Claude Code session only for `ui_bug` / `api_bug`.

## Debugging Tips

1. **Test hangs at `queued` phase >10s** → Sirin may be stuck spawning Chrome. Check Sirin logs for browser launch errors.

2. **Always returns `failed` with "LLM error"** → Call `config_diagnostics` MCP tool to check LLM/router health from the outside (no need to open Sirin GUI).

3. **Screenshot returns `bytes_base64: null`** → Look at `screenshot_error` field. Common causes:
   - Flutter CanvasKit in headless mode → blank PNG
   - Page closed before screenshot captured

4. **Truncation hint never resolves** → Make sure to use `get_full_observation` with the exact `step` index from the hint message.

5. **`run_id not found`** → Completed runs are pruned after 1 hour. Re-run if you need the data.

6. **`ax_find` returns wrong node** → Use `name_regex` for precise matching, e.g. `name_regex:"^Balance$"`. Add `not_name_matches:["Header","Label"]` to exclude decorative nodes with similar text.

7. **`ax_snapshot` ID not found on ax_diff** → Snapshots live in memory; they reset on Sirin restart. Retake the snapshot.

8. **AuthZ Ask hangs 30s then denies** → Monitor tab is closed. Open it and retry, or switch to `permissive` mode in `config/authz.yaml`.

9. **bash curl fails with CJK/Unicode** (e.g. `name=登入` corrupted in shell) → use the `sirin-call` CLI instead:
   ```bash
   # Build once:
   cargo build --release   # → target/release/sirin-call.exe
   # CJK-safe key=value:
   sirin-call browser_exec action=ax_find role=button name=登入
   # Or pipe JSON (no shell escaping needed):
   echo '{"action":"ax_find","role":"button","name":"購買"}' | sirin-call browser_exec
   # List tools:
   sirin-call --list
   ```

10. **`ax_tree` returns only 1-2 nodes after `enable_a11y`** → Flutter tree is still building.
    Use `wait_for_ax_ready` instead of a fixed `wait`:
    ```
    browser_exec({action: "wait_for_ax_ready", min_nodes: 20, timeout_ms: 8000})
    ```
    Sirin also auto-retries internally (polls 3×400ms then re-triggers bootstrap), but
    explicit `wait_for_ax_ready` gives you control over the timeout and is more reliable
    in test scripts.

## Anti-patterns

❌ **Don't use Sirin for single-browser-action tasks** — Just use the raw `web_navigate` tool if Sirin's agent tools are accessible. Sirin test runner has ~3s overhead per step.

❌ **Don't write tests as step-by-step scripts** — Goal-driven is the whole point. If you want scripts, use Puppeteer.

❌ **Don't enable `auto_fix=true` on experimental tests** — Will waste Claude Code tokens trying to "fix" things that aren't bugs.

❌ **Don't block on synchronous `run_test`** — Always use `run_test_async` + poll. Tests can take 2+ minutes.

❌ **Don't store `ax_snapshot` IDs across Sirin restarts** — They're in-process memory only; retake snapshots at the start of each session.

## Example: Full Session

```
User: "Run the Agora Market login test and if it fails, let Claude fix it"

Claude Code:
1. list_tests(tag="auth")
   → { tests: [{id: "login_flow", ...}] }

2. run_test_async(test_id="login_flow", auto_fix=true)
   → { run_id: "run_abc123", status: "queued" }

3. poll get_test_result every 5s...
   running → running → failed

4. get_test_result(run_id="run_abc123")
   → details.error="Login button not responding after click"
   → failure_category inferred: "ui_bug"

5. get_screenshot(run_id="run_abc123")
   → [shows login page with error toast]

6. User sees: "Test failed — UI bug detected. Sirin auto-spawned a
   Claude Code session in the frontend repo to investigate."

7. Check frontend repo in a minute for a commit / PR.
```
