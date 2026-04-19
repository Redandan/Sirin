# Agora Market — Sirin Browser Test Cheatsheet

Agora Market is a Flutter Web (CanvasKit) PWA hosted at
`https://redandan.github.io/`.  It is Sirin's canonical complex test target.
Every Flutter-specific pitfall that Sirin has already solved is documented
here so new AI sessions don't rediscover them.

---

## 1. Why `browser_headless: false` Is Required

Flutter Web's CanvasKit renderer uses WebGL to paint.  Chrome's headless mode
does **not** expose a real GPU/WebGL context, so the app canvas stays blank —
every screenshot is a black rectangle.

Attempting to use `screenshot_analyze` or vision-based assertions without this
flag will silently fail: the LLM will always report "blank page" and the test
will either time-out or produce a false negative.

**Always set this field in any test YAML targeting Agora Market:**

```yaml
browser_headless: false   # ← required for CanvasKit / WebGL paint
```

The field is parsed by `src/test_runner/parser.rs` and passed to Sirin's
browser singleton before the first `navigate` call.  Without it Chrome starts
in headless mode (the default) and the canvas never paints.

> Reference: `src/test_runner/parser.rs` — `browser_headless` field  
> Reference: `config/tests/agora_market_smoke.yaml` — working example

---

## 2. Hash-Route Navigation

Agora Market is a Single-Page App that uses URL fragments for routing:

| Route | URL |
|-------|-----|
| Login / Register | `https://redandan.github.io/#/login` |
| Dashboard / Home | `https://redandan.github.io/#/` or `#/home` |
| Wallet | `https://redandan.github.io/#/wallet` |
| Products | `https://redandan.github.io/#/products` |

### Why naive navigation hangs

`headless_chrome`'s `wait_until_navigated()` waits for the CDP
`Page.frameNavigated` event.  Chrome **does not** emit `frameNavigated` for
pure fragment changes — only for full page loads.  Naively calling
`navigate_to("https://redandan.github.io/#/wallet")` from `#/home` would
block forever waiting for an event that never arrives.

### How Sirin handles it automatically

`src/browser.rs` contains an `is_hash_only_change()` helper (line 275) that
compares the current URL against the target.  When only the fragment differs
(same origin, same path, same query), Sirin uses `location.hash =` assignment
via JS evaluation instead of a CDP navigate call, which does not require a
`frameNavigated` event:

```rust
// src/browser.rs ~line 228
if is_hash_only_change(&current, url) {
    let hash = url.split_once('#').map(|(_, h)| h).unwrap_or("");
    evaluate_js(&format!("location.hash = {};", ...))
}
```

**You do not need to handle this yourself.**  Call `goto` with the full URL
including the fragment; Sirin's `navigate` function detects and handles it.

---

## 3. The K14 ax_* Selector Pattern

### Why CSS / DOM selectors don't work in Flutter Web

Flutter Web renders into a single `<flt-glass-pane>` canvas element.  The
DOM contains no semantic HTML — no `<button>`, no `<input>`, no text nodes
that CSS selectors can find.  DOM-based actions (`click`, `exists`, `eval`
targeting text) all silently fail or return empty results.

### The workaround: CDP Accessibility Tree

Flutter Web exposes an accessibility tree via Chrome's `Accessibility` CDP
domain.  Sirin reads this tree and identifies elements by their **literal
accessible name** (role + text string), not by CSS.

**Typical pattern for interacting with a Flutter widget:**

```
Step 1 — ax_find: locate the node by role and accessible name
  {"action": "ax_find", "role": "button", "name": "登入"}
  → returns {"found": true, "node": {"backend_id": 42, "role": "button", "name": "登入"}}

Step 2 — ax_click: click it by backend_id
  {"action": "ax_click", "backend_id": 42}

Step 3 — verify: ax_find or ax_value to confirm result
  {"action": "ax_find", "role": "text", "name_regex": "歡迎|Welcome"}
```

**Available ax_* actions (all via `web_navigate` tool):**

| Action | Key Args | Purpose |
|--------|----------|---------|
| `ax_tree` | `include_ignored?` | Dump full tree (debugging) |
| `ax_find` | `role?`, `name?`, `name_regex?` | Find first matching node |
| `ax_click` | `backend_id` | Click node (Flutter-compatible 5-event sequence) |
| `ax_value` | `backend_id` | Read literal text from node |
| `ax_focus` | `backend_id` | Focus an input node |
| `ax_type` | `backend_id`, `text` | Type text into focused input |
| `ax_type_verified` | `backend_id`, `text` | Type + verify round-trip |
| `ax_snapshot` | `id?` | Save current tree snapshot |
| `ax_diff` | `before_id`, `after_id` | Diff two snapshots |

> Reference: `src/adk/tool/builtins.rs` lines 836–936  
> Reference: `src/browser_ax.rs` — `find_by_role_and_name`, `click_backend`

### RawGetFullAxTree workaround

`headless_chrome` 1.0.x deserializes CDP responses strictly.  Newer Chrome
versions emit `AXPropertyName` values (e.g. `uninteresting`) that the crate's
enum does not include.  This would panic the entire `getFullAXTree` call.

Sirin bypasses this with a custom `RawGetFullAxTree` struct that declares its
return type as `serde_json::Value` (raw JSON) instead of the crate's typed
struct (`src/browser_ax.rs:43`):

```rust
struct RawGetFullAxTree {}
impl Method for RawGetFullAxTree {
    const NAME: &'static str = "Accessibility.getFullAXTree";
    type ReturnObject = serde_json::Value;   // ← raw; unknown fields ignored
}
```

You don't need to do anything — this is baked into `get_full_tree()`.  The
pattern is documented here so you can apply it to other CDP methods that show
the same symptom.

---

## 4. Flutter Semantics Tree Collapse

### Symptom

After `Accessibility.enable` + the first `getFullAXTree`, the tree sometimes
contains only 1–2 nodes (`RootWebArea` only) instead of the full widget tree.

### Two distinct root causes

| Cause | Symptom | Resolution |
|-------|---------|------------|
| **Cold start** — Flutter has not activated its a11y bridge yet | Tree has 1 node after page load | Placeholder click bootstraps the bridge |
| **Post-navigation rebuild** — Flutter tearing down + rebuilding widgets | Tree briefly collapses, then recovers | Wait ~1 second; tree self-recovers |

### What Sirin does automatically (`src/browser_ax.rs:191`)

```rust
if !poll_tree_recovery(3, 400) {   // poll 3×400ms — allow self-recovery first
    let _ = enable_flutter_semantics();   // cold-start bootstrap if still empty
}
```

`poll_tree_recovery` checks every 400ms (×3) whether the tree already has more
than 2 nodes — this catches situation 2 (post-nav rebuild) without
unnecessarily triggering the bootstrap click.  Only if the tree is still empty
after 1.2 seconds does it call `enable_flutter_semantics` (a placeholder click
to force the a11y bridge on).

### If you need to wait explicitly

Use `wait_for_ax_ready` before issuing `ax_find` after a navigation:

```
{"action": "wait_for_ax_ready", "min_nodes": 20, "timeout_ms": 5000}
```

This blocks until ≥20 nodes are present or 5 seconds elapse.

> Reference: `src/browser_ax.rs:247` — `poll_tree_recovery`  
> Reference: `src/browser_ax.rs:280` — `wait_for_ax_ready`  
> Reference: `src/browser_ax.rs:656` — `enable_flutter_semantics`

---

## 5. Working YAML Example

Below is a complete test goal that exercises hash-route navigation,
`browser_headless: false`, and the ax_* pattern.  The existing smoke test
(`config/tests/agora_market_smoke.yaml`) uses vision only; this example
uses the accessibility tree for exact assertions.

```yaml
id: agora_login_ax
name: "Agora Market — ax_* login flow test"
url: "https://redandan.github.io/#/login"
browser_headless: false          # CanvasKit WebGL paint requires visible Chrome

goal: |
  Verify the Agora Market login page is functional using the accessibility tree.

  Steps:
  1. Navigate to the login page (#/login).
  2. Call wait_for_ax_ready with min_nodes=20, timeout_ms=8000 to ensure
     Flutter semantics are loaded.
  3. Use ax_find to locate the email input field (role="textbox" or similar).
  4. Use ax_focus + ax_type to enter "test@example.com".
  5. Use ax_find to locate the password field and type "password123".
  6. Use ax_find to locate the login/submit button and ax_click it.
  7. Wait 2000ms for navigation, then check the URL or ax_find for a
     post-login indicator (dashboard element, user avatar, etc.).

  Report PASS if a post-login UI element is found.
  Report FAIL if ax_find returns found=false for expected elements.

max_iterations: 20
timeout_secs: 120

success_criteria:
  - "ax_find confirms email input field is present on login page"
  - "ax_find confirms a post-login element appears after submit"

tags: [auth, ax, flutter, agora]
```

> Note: The exact `role` and `name` values depend on what Flutter exposes.
> Run `{"action": "ax_tree"}` first to inspect the live tree and discover
> the right selectors before writing assertions.

---

## Quick Reference

```
# Always start with this when testing Agora Market:
browser_headless: false

# Wait for Flutter semantics after navigate:
{"action": "wait_for_ax_ready", "min_nodes": 20, "timeout_ms": 8000}

# Discover widget tree:
{"action": "ax_tree"}

# Find a button by text:
{"action": "ax_find", "role": "button", "name": "登入"}

# Click it:
{"action": "ax_click", "backend_id": <id from ax_find>}

# Read a value:
{"action": "ax_value", "backend_id": <id>}

# Navigate to a hash route (Sirin handles the hash-only path automatically):
{"action": "goto", "target": "https://redandan.github.io/#/wallet"}
```

---

## Files Referenced

| File | What |
|------|------|
| `src/browser.rs:275` | `is_hash_only_change()` — hash-route detection |
| `src/browser_ax.rs:43` | `RawGetFullAxTree` — strict-enum workaround |
| `src/browser_ax.rs:191` | `get_full_tree` — auto-bootstrap + poll logic |
| `src/browser_ax.rs:247` | `poll_tree_recovery` — 3×400ms self-recovery poll |
| `src/browser_ax.rs:280` | `wait_for_ax_ready` — explicit tree-ready wait |
| `src/browser_ax.rs:656` | `enable_flutter_semantics` — cold-start bootstrap |
| `src/adk/tool/builtins.rs:836` | ax_* action dispatch |
| `src/test_runner/parser.rs` | `browser_headless` YAML field |
| `config/tests/agora_market_smoke.yaml` | Vision-based smoke test example |
