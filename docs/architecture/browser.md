# Browser Layer Architecture

> **Files:** `src/browser.rs`, `src/browser_ax.rs`
> **Role:** Persistent Chrome CDP singleton — navigation, interaction,
> network capture, multi-tab, named sessions, and accessibility tree.

---

## 1. Purpose

`browser.rs` provides a **single, persistent Chrome DevTools Protocol (CDP)**
session shared across the entire Sirin process.  The design goal is zero
per-call overhead: the browser stays open between test steps, tool calls, and
MCP requests.  When Chrome crashes or the WebSocket connection drops, the
singleton self-heals before the next operation.

`browser_ax.rs` builds on top to expose the **CDP Accessibility tree** as
structured data.  This is the layer that turns "about 7377 USDT" (screenshot
description) into the literal `$7376.80` (a11y `name` field) — critical for
K14/K15-style assertions.

Together they expose **73 public actions** organised in capability tiers.

---

## 2. Action Catalog

### Navigation

| Function | Description |
|---|---|
| `navigate(url)` | Full navigation with retry on transient CDP errors |
| `navigate_and_screenshot(url)` | Navigate + return PNG bytes |
| `current_url()` | Live URL via `Runtime.evaluate("window.location.href")` |
| `page_title()` | Live title via `Runtime.evaluate("document.title")` |
| `wait_for_navigation()` | Block until `Page.frameNavigated` fires |
| `wait_for_url(target, ms)` | Poll until URL contains substring or matches regex |

### DOM Interaction — Tier 1 (Selector-based)

| Function | Description |
|---|---|
| `click(selector)` | `wait_for_element` + `.click()` |
| `type_text(selector, text)` | Focus element then `type_into` |
| `press_key(key)` | Single key press (`"Enter"`, `"Tab"`, `"Escape"`, …) |
| `press_key_combo(key, modifiers)` | Key + modifier(s): `"ctrl"`, `"shift"`, `"alt"`, `"meta"` |
| `select_option(selector, value)` | Set `<select>` value + dispatch `change` event |
| `scroll_by(x, y)` | `window.scrollBy(x, y)` |
| `scroll_into_view(selector)` | `element.scrollIntoView({behavior:'smooth',block:'center'})` |
| `wait_for(selector)` | Wait for CSS selector to appear |
| `wait_for_ms(selector, ms)` | Same with custom timeout |

### DOM Interaction — Tier 2 (Coordinate-based)

| Function | Description |
|---|---|
| `click_point(x, y)` | CDP `Input.dispatchMouseEvent` at viewport coords |
| `hover_point(x, y)` | Mouse move to (x, y) — triggers hover effects |
| `hover(selector)` | `move_mouse_over()` on matched element |
| `drag(from_x, from_y, to_x, to_y)` | mousePressed → mouseMoved → mouseReleased |

### Introspection

| Function | Description |
|---|---|
| `screenshot()` | Full-tab PNG |
| `screenshot_jpeg(quality)` | Full-tab JPEG (JPEG 80 ≈ 10× smaller than PNG) |
| `screenshot_element(selector)` | PNG cropped to element's content box |
| `get_text(selector)` | `innerText` of matched element |
| `get_attribute(selector, attr)` | `querySelector(sel).getAttribute(attr)` |
| `get_value(selector)` | `querySelector(sel).value` (form elements) |
| `element_exists(selector)` | Returns `bool`; never errors on absence |
| `element_count(selector)` | Count of matching elements |
| `evaluate_js(expr)` | Eval arbitrary JS, coerce return to `String` |
| `get_content()` | Full page HTML |
| `console_messages(limit)` | Read from `window.__sirin_console` ring buffer |
| `diagnostic_snapshot()` | Chrome version, headless flag, tab count, session names |

### Browser State

| Function | Description |
|---|---|
| `ensure_open(headless)` | Launch Chrome (or re-launch on mode mismatch) |
| `ensure_open_reusing()` | Open if closed, preserve mode if already open |
| `close()` | Drop singleton, close Chrome |
| `is_open()` | Boolean health check |
| `set_viewport(w, h, scale, mobile)` | `Emulation.setDeviceMetricsOverride` + cache |
| `clear_browser_state()` | Wipe cookies + localStorage + sessionStorage + IndexedDB + caches |
| `install_console_capture()` | Monkey-patch `console.log/warn/error/info` |
| `local_storage_get(key)` | Read from `localStorage` |
| `local_storage_set(key, value)` | Write to `localStorage` |
| `set_http_auth(user, pass)` | `Network.setExtraHTTPHeaders` Authorization: Basic |
| `file_upload(selector, paths)` | `DOM.setFileInputFiles` |
| `iframe_eval(selector, expr)` | `contentWindow.eval(expr)` inside same-origin iframe |
| `pdf()` | `Page.printToPDF` — headless only |

### Network

| Function | Description |
|---|---|
| `network_requests(limit)` | `performance.getEntriesByType('resource')` summary |
| `install_network_capture()` | Monkey-patch `fetch` + `XMLHttpRequest` to capture req+res body |
| `captured_requests(limit)` | Read from `window.__sirin_net` ring buffer |
| `wait_for_request(url_substr, ms)` | Block until a matching request lands in the buffer |
| `wait_for_network_idle(idle_ms, ms)` | Block until no new requests for `idle_ms` |

### Cookies

| Function | Description |
|---|---|
| `get_cookies()` | All cookies → JSON array |
| `set_cookie(name, value, domain, path)` | `Network.setCookies` |
| `delete_cookie(name)` | `Network.deleteCookies` on current URL |

### Multi-Tab

| Function | Description |
|---|---|
| `new_tab()` | Open new tab, return index |
| `switch_tab(index)` | Set active by index |
| `close_tab(index)` | Remove tab (cannot close last) |
| `list_tabs()` | `Vec<(index, url)>` |
| `active_tab()` | Current active index |
| `wait_for_new_tab(baseline, ms)` | Poll until Chrome tab count grows (OAuth popups, `window.open`) |

### Named Sessions

| Function | Description |
|---|---|
| `session_switch(session_id)` | Create or activate a named session (opens a new tab if needed) |
| `list_sessions()` | `Vec<(session_id, tab_index, url)>` |
| `close_session(session_id)` | Close tab + remove name mapping |

### Accessibility Tree (`browser_ax`)

| Function | Description |
|---|---|
| `enable()` | `Accessibility.Enable` — idempotent per-tab |
| `get_full_tree(include_ignored)` | Full AX tree as `Vec<AxNode>` |
| `find_by_role_and_name(role, name, regex, exclude)` | First matching node |
| `find_all_by_role_and_name(...)` | Multi-match with limit |
| `find_scrolling_by_role_and_name(...)` | Scroll-aware: scrolls down until node appears |
| `ax_snapshot(id)` | Capture + store named AX snapshot |
| `ax_diff(before_id, after_id)` | Diff two stored snapshots (added / removed / changed) |
| `wait_for_ax_change(baseline_id, ms)` | Poll until tree diverges from baseline |
| `wait_for_ax_ready(min_nodes, ms)` | Poll until tree has at least N nodes |
| `click_backend(backend_id)` | 5-event PointerEvent+MouseEvent sequence via JS (Flutter-safe) |
| `focus_backend(backend_id)` | `DOM.focus` on backend node |
| `type_into_backend(backend_id, text)` | Focus + `Input.insertText` |
| `type_into_backend_verified(backend_id, text)` | Type + 300ms settle + readback verify |
| `read_node_text(backend_id)` | Read `value` or `name` from AX node |
| `enable_flutter_semantics()` | Bootstrap Flutter a11y bridge (strategies A/B) |

---

## 3. Singleton + Auto-Reconnect

```
static SESSION: OnceLock<Arc<Mutex<Option<BrowserInner>>>> = OnceLock::new();
```

The `OnceLock` is initialised once to an `Arc<Mutex<Option<BrowserInner>>>`.
The `Option` starts as `None` (no browser) and is set by `ensure_open()`.
Closing the browser sets it back to `None`.

**`BrowserInner`** holds:
- `browser: Browser` — the headless_chrome process handle
- `tabs: Vec<Arc<Tab>>` — all open tabs (tab 0 is always present)
- `active: usize` — index of the active tab
- `headless: bool` — mode the session was launched in
- `sessions: HashMap<String, usize>` — named session → tab index
- `viewport: Option<(u32, u32, f64, bool)>` — cached last `set_viewport` call

**`with_tab(f)`** is the central dispatch primitive:

1. Calls `ensure_open_reusing()` (no-op if already open).
2. Acquires the mutex, calls `f(tab)`.
3. If `f` fails with a connection-closed error (`"underlying connection is closed"` /
   `"TaskCancelled"` / `"ChannelClosed"`), it clears the singleton, relaunches,
   and retries exactly once.

The mutex is held only for the duration of one CDP call.  `tokio::task::spawn_blocking`
is used by async callers to keep the blocking mutex off the async executor.

**Mode switching**: `ensure_open(headless)` compares the requested mode against
`inner.headless`.  On mismatch it drops `*guard = None` and re-launches.  A
600ms settle delay after launch lets Chrome's frame tree and CDP event
subscriptions stabilise before the first `navigate_to` call.

---

## 4. Notable Workarounds

### `RawGetFullAxTree` — strict-enum bypass (`browser_ax.rs:43`)

`headless_chrome 1.0.21` deserialises `Accessibility.getFullAXTree` responses
into a typed `AXNode` struct whose `AXPropertyName` enum is incomplete.  Chrome
now returns a property named `"uninteresting"` that the crate doesn't know about,
causing a hard deserialisation failure for the entire response.

**Fix**: a custom zero-field struct `RawGetFullAxTree` implements
`headless_chrome::protocol::cdp::types::Method` with
`type ReturnObject = serde_json::Value`.  The raw JSON is then walked manually
to pull only the fields Sirin needs (`nodeId`, `backendDOMNodeId`, `role`,
`name`, `value`, `description`, `childIds`, `ignored`).

### Flutter Semantics Retrigger — `enable_flutter_semantics()` (`browser_ax.rs:656`)

Flutter Web only activates its a11y bridge when it detects an AT.  Three
strategies are attempted in order:

- **Strategy A** — look for a node named `"enable accessibility"` in the raw
  (possibly collapsed) tree and `click_backend` it.  Flutter 3.x+ clean builds
  surface this button explicitly.
- **Strategy B** — `querySelector('flt-semantics-placeholder').click()`.
  If the element has been removed (idle-collapse), it is re-created in JS
  before clicking.
- **Strategy C — permanently removed** (Issue #20): the original `Tab×2`
  fallback sent keyboard events that Flutter's active router intercepted,
  resetting the page URL to `about:blank` on any page visited after a
  `click_backend` navigation.  Callers now use `wait_for_ax_ready` instead.

**`enable()` idempotency guard** (`browser_ax.rs:148`): resending
`Accessibility.Enable` to a Flutter tab that has idle-collapsed its semantics
tree causes the same `about:blank` reset.  A `static A11Y_ENABLED_TABS:
OnceLock<Mutex<HashSet<usize>>>` tracks which tab indices have already received
the command.  The set is cleared on Chrome re-launch (`reset_a11y_enabled()`)
and on tab close (`remove_a11y_tab(index)`).

### Hash-Route Fast Path — `is_hash_only_change()` (`browser.rs:275`)

`headless_chrome` waits for `Page.frameNavigated` after `navigate_to()`.
Chrome does **not** emit `frameNavigated` for pure fragment (`#hash`) changes,
causing a 60-second timeout on every SPA hash route transition.

**Fix**: `navigate_with_retry()` compares the base URLs (origin + path + query)
of the current and target URLs.  If only the fragment differs, navigation is
done via `location.hash = "..."` JS assignment + 400ms settle, bypassing
`wait_until_navigated` entirely.

### Mode-Switch Settle Delay (`browser.rs:172`)

After `Browser::new()` + `browser.new_tab()`, Chrome reports the tab as ready
immediately.  CDP event subscriptions (`Page.frameNavigated`, etc.) need ~500ms
to stabilise.  Without the explicit `thread::sleep(600ms)`, the first
`navigate_to` misses the frame event and returns
`"The event waited for never came"`.

**`navigate_with_retry(url, 2)`** provides a one-shot retry with 500ms wait
specifically for this transient error, so the mode-switch path is robust without
requiring callers to add their own delay.

### `current_url()` and `page_title()` via `Runtime.evaluate` (`browser.rs:331`)

`Tab::get_url()` and `Tab::get_title()` read a cache populated by
`Target.targetInfoChanged` events.  Chrome does not fire this event for:
- Hash-only navigation
- `document.title` mutated by JavaScript
- `window.location.replace("about:blank")`

**Fix**: both helpers call `tab.evaluate("window.location.href")` /
`tab.evaluate("document.title")` respectively.  Fallback to the cached values
if `Runtime.evaluate` fails (no execution context, debugger paused).

### Flutter `click_backend` — 5-Event Sequence (`browser_ax.rs:534`)

CDP `Input.dispatchMouseEvent` synthesises only `MouseEvent` (`mousedown` /
`mouseup` / `click`).  Flutter 3.13+ gesture detectors require the complete
sequence including `PointerEvent` (`pointerdown` / `pointerup`).  Without
pointer events the tap is silently dropped — `click_backend` returned success
but the route never changed (Issue #22-3).

**Fix**: `click_backend` resolves the element centre via `DOM.getBoxModel`,
then injects a JavaScript snippet that dispatches the full
`pointerdown → mousedown → pointerup → mouseup → click` sequence via
`Element.dispatchEvent`.  Using `document.elementFromPoint(cx, cy)` as the
target ensures the hit-test is correct for both Flutter Canvas and plain DOM.

---

## 5. Network Capture

`install_network_capture()` injects a JavaScript shim at runtime:

- **`fetch` monkey-patch**: wraps the original `window.fetch` to capture
  request method, URL, serialised body (string, FormData → JSON object,
  URLSearchParams → string, Blob/ArrayBuffer → `[binary N bytes]`), HTTP
  status, and response body (capped at 4 000 chars each).
- **`XMLHttpRequest` monkey-patch**: patches `XHR.prototype.open` and `.send`
  to capture the same fields via the `load` event listener.
- Captured entries are stored in `window.__sirin_net` (ring buffer, cap 100).

`wait_for_request(url_substring, timeout_ms)` polls `window.__sirin_net` every
150ms until a matching entry appears.  This solves the race between "click
submit button" and "read what the API was called with" — without it the
captured_requests read lands before the POST fires.

**Why JavaScript instead of CDP `Network.enable`**: CDP network events are
process-wide and require all responses to be buffered in the CDP transport.  On
high-traffic pages (trading terminals) this adds hundreds of MB of CDP traffic.
The JS shim is opt-in, per-page, and limited to the ring buffer cap.

---

## 6. Multi-Tab + Named Sessions

### Unnamed tabs

The `BrowserInner.tabs: Vec<Arc<Tab>>` vector mirrors every tab Sirin knows
about.  `new_tab()` calls `browser.new_tab()` and appends to the vector.
`switch_tab(i)` sets `inner.active = i`.  `close_tab(i)` removes from the
vector and adjusts `inner.active` if it was pointing past the removed slot.

**`wait_for_new_tab(baseline, ms)`** handles tabs created outside Sirin's
control (OAuth popups, `window.open`, `target="_blank"` clicks).  It polls
`inner.browser.get_tabs()` (the underlying headless_chrome browser-level list)
every 200ms.  Once the count exceeds `baseline`, it calls
`browser.register_missing_tabs()` to sync the browser's internal tab registry,
then adopts the new tab into `inner.tabs` and makes it active.

### Named sessions

`BrowserInner.sessions: HashMap<String, usize>` maps a caller-chosen string
ID to a tab index.

```
session_switch("buyer_a")  →  create new tab, record sessions["buyer_a"] = 2
session_switch("buyer_b")  →  create new tab, record sessions["buyer_b"] = 3
session_switch("buyer_a")  →  inner.active = 2  (no new tab)
```

All subsequent `with_tab` calls target `inner.tabs[inner.active]` so session
isolation is transparent to every action above it.  Typical use: cross-role
E2E tests where buyer and seller need independent browser contexts without
running two Chrome processes.

`close_session(id)` removes the tab from the vector, decrements session indices
that pointed past the removed slot, and calls `browser_ax::remove_a11y_tab(i)`
to keep the a11y-enabled tracker in sync.

---

## 7. Known Limits / Future Work

| Issue | Detail |
|---|---|
| **Flutter CanvasKit blank in headless** | Flutter's WebGL renderer paints all-black in `--headless=new` (Chrome 112+) because there is no GPU.  Tests that need to see Flutter content must set `headless=false` (or `browser_headless: false` in the YAML goal).  `ensure_open()` detects the mode mismatch and re-launches. |
| **Windows `TIME_WAIT` on rapid relaunch** | Killing and immediately relaunching Chrome on Windows can leave the CDP WebSocket port in `TIME_WAIT` for 30–120s.  `dev-relaunch.sh` works around this with a brief `sleep 3` before relaunch. |
| **Chrome 147 `--load-extension` blocked** | The planned Sirin Companion extension (to read live tab state via `chrome.tabs.*`) is non-functional.  Chrome 122 deprecated `--load-extension` for non-Chrome-for-Testing builds; Chrome ~147 removed the opt-out feature flag.  `locate_companion_ext()` and `ext/` are kept as stubs pending a Chrome for Testing sidecar. |
| **`headless_chrome` strict enums** | The `RawGetFullAxTree` workaround will become unnecessary if/when the crate upstream adds `serde(other)` to `AXPropertyName`.  Tracked in the crate's issue list. |
| **Network capture misses Service Worker requests** | `install_network_capture` patches `window.fetch` and `XMLHttpRequest`.  Requests made directly from a Service Worker bypass both patches and will not appear in `window.__sirin_net`. |
| **Viewport not re-applied to unnamed new tabs** | `reapply_viewport()` is called after `navigate`, `clear_browser_state`, and `wait_for_new_tab`.  Tabs opened by `new_tab()` directly (not via `wait_for_new_tab`) also re-apply via `wait_for_new_tab`'s adoption path, but a direct `new_tab()` + `switch_tab()` does not trigger re-application — caller must call `set_viewport` again. |
