---
name: sirin-test
description: This skill should be used when the user asks to "test a website", "run an E2E test", "verify a user flow", "check if a page works", "run browser tests", or mentions Sirin's testing capabilities. Provides the workflow for driving Sirin's AI-powered browser test runner via its MCP API (:7700/mcp).
version: 1.1.0
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

**→ FIX: `screenshot_analyze` action + Gemini Vision** reads the
rendered canvas pixels.

```yaml
goal: |
  ⚠️ Flutter CanvasKit app. DOM is empty — use screenshot_analyze:
    {action:"screenshot_analyze",
     target:"Does the page show the login form with an email field?"}
  Don't try eval/read/exists for text content.

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
