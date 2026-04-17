---
name: sirin-test
description: This skill should be used when the user asks to "test a website", "run an E2E test", "verify a user flow", "check if a page works", "run browser tests", or mentions Sirin's testing capabilities. Provides the workflow for driving Sirin's AI-powered browser test runner via its MCP API (:7700/mcp).
version: 1.3.0
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
| `get_test_result` | Poll run status by `run_id` | Every 3-5s while test runs |
| `get_screenshot` | Fetch base64 PNG of failure | status=failed/timeout/error |
| `get_full_observation` | Un-truncated tool output | Observation mentions `[truncated: ...]` |
| `list_recent_runs` | Historical test executions (SQLite) | Debug flakiness / see patterns |
| `list_fixes` | Auto-fix history (claude_session spawns) | Check if a fix is in-flight |
| `config_diagnostics` | LLM/router/vision health report | Tests failing mysteriously — self-diagnose |
| `browser_exec` | Single imperative browser action | **Debug / one-off exploration without a goal** |

`browser_exec` accepts these `action` values:

**Standard:** `goto`, `screenshot`, `screenshot_analyze`, `click`, `click_point`, `type`, `read`, `eval`, `wait`, `exists`, `attr`, `scroll`, `key`, `console`, `network`, `url`, `title`, `close`, `set_viewport`

**Accessibility tree** (literal string, no vision approximation — **use these for K14/K15 exact-value assertions**):
- `enable_a11y` — trigger Flutter semantics bridge (call before `ax_tree` on Flutter Canvas apps)
- `ax_tree` — full a11y node list with `role`, literal `name`, literal `value`, `backend_id`, `child_ids`
- `ax_find` (params: `role`, `name`) — single match by role + name substring
- `ax_value` (param: `backend_id`) — exact text content (`value || name`)
- `ax_click` / `ax_focus` (param: `backend_id`) — interaction by DOM backend id
- `ax_type` (params: `backend_id`, `text`) — focus + insertText
- `ax_type_verified` (same params) — types then reads back, returns `{typed, actual, matched}` so you know if Flutter dropped chars or the input formatted the value

**Robustness** (test isolation, race-free assertions, popups):
- `clear_state` — wipe cookies + localStorage + sessionStorage + IndexedDB + caches between tests so K13 can't leak auth into K14
- `wait_request` (params: `target` URL substring, `timeout` ms default 10000) — block until a fetch/XHR matching the substring is captured; auto-installs network capture; eliminates click-then-read races; returns the entry **including `req_body`**
- `wait_new_tab` (param: `timeout` ms default 10000) — block until a popup/OAuth tab opens; auto-discovers via `register_missing_tabs` and switches `active` to the new tab

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
   → { found: true, node: { backend_id: 142, name: "Total Assets",
                            role: "StaticText", ... } }

4. browser_exec({action: "ax_value", backend_id: 142})
   → { backend_id: 142, text: "$7376.80" }    ← LITERAL string

5. (do something that changes the balance)

6. browser_exec({action: "ax_value", backend_id: 142})
   → { backend_id: 142, text: "$7277.50" }    ← LITERAL string

7. assert: 7376.80 - 7277.50 == 99.30  ← exact, not "about"
```

**Tips:**
- For Flutter apps, `enable_a11y` is required to wake the semantics
  tree. Sirin auto-retries this if a subsequent `ax_tree` returns
  ≤2 nodes (Flutter periodically collapses the tree).
- `ax_find` `name` is **substring + case-insensitive**. Pass exact
  text in `ax_value` for the precise comparison.
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
       → backend_id of the balance display
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

## Anti-patterns

❌ **Don't use Sirin for single-browser-action tasks** — Just use the raw `web_navigate` tool if Sirin's agent tools are accessible. Sirin test runner has ~3s overhead per step.

❌ **Don't write tests as step-by-step scripts** — Goal-driven is the whole point. If you want scripts, use Puppeteer.

❌ **Don't enable `auto_fix=true` on experimental tests** — Will waste Claude Code tokens trying to "fix" things that aren't bugs.

❌ **Don't block on synchronous `run_test`** — Always use `run_test_async` + poll. Tests can take 2+ minutes.

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
