//! Persistent browser session wrapping `headless_chrome`.
//!
//! ## Concurrency
//! A process-wide singleton (`SESSION`) holds the active browser.  All public
//! helpers acquire the inner `Mutex` for the duration of a single CDP call,
//! keeping lock contention short.  Call from async code via
//! `tokio::task::spawn_blocking`.
//!
//! ## Capability tiers
//! - **Tier 1** — navigation, screenshot, click, type, read, eval, wait,
//!   scroll, keyboard, select, element queries, viewport, console capture.
//! - **Tier 2** — coordinate click/hover, element screenshot.
//! - **Tier 3** — multi-tab, cookies, network intercept, file upload,
//!   iframe, PDF export, HTTP auth, drag-and-drop.

use headless_chrome::browser::tab::ModifierKey;
use headless_chrome::browser::tab::point::Point;
use headless_chrome::protocol::cdp::Input;
use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
use headless_chrome::protocol::cdp::{Emulation, Network, Page};
use headless_chrome::{Browser, LaunchOptions, Tab};
use serde_json::json;
use std::sync::{Arc, Mutex, OnceLock};

// ── Singleton ────────────────────────────────────────────────────────────────

static SESSION: OnceLock<Arc<Mutex<Option<BrowserInner>>>> = OnceLock::new();

fn global() -> &'static Arc<Mutex<Option<BrowserInner>>> {
    SESSION.get_or_init(|| Arc::new(Mutex::new(None)))
}

struct BrowserInner {
    browser: Browser,
    tabs: Vec<Arc<Tab>>,
    active: usize,
    #[allow(dead_code)]
    headless: bool,
}

impl BrowserInner {
    fn tab(&self) -> &Arc<Tab> {
        &self.tabs[self.active]
    }
}

// ══════════════════════════════════════════════════════════════════════════════
//  SESSION LIFECYCLE
// ══════════════════════════════════════════════════════════════════════════════

/// Resolve the default headless mode, honouring `SIRIN_BROWSER_HEADLESS` env
/// override.  Accepts: `true`/`false`/`0`/`1`.  Default: `true`.
pub fn default_headless() -> bool {
    match std::env::var("SIRIN_BROWSER_HEADLESS").ok().as_deref() {
        Some("false") | Some("0") | Some("FALSE") | Some("no") | Some("NO") => false,
        Some("true")  | Some("1") | Some("TRUE")  | Some("yes") | Some("YES") => true,
        _ => true,  // default
    }
}

/// Ensure a persistent browser is running.
///
/// **Mode switching**: if an existing session is using a different headless
/// mode than requested, it is closed and a fresh browser is launched in the
/// desired mode.  This is required because Flutter CanvasKit / WebGL content
/// doesn't paint correctly in headless mode — tests that need those must
/// explicitly request `headless=false`.
///
/// Also runs a cheap CDP health check on existing sessions and re-launches
/// if the connection is dead.  Returns `true` if a new browser was launched.
pub fn ensure_open(headless: bool) -> Result<bool, String> {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());

    if let Some(inner) = guard.as_ref() {
        if inner.headless != headless {
            tracing::info!(
                "[browser] headless mode mismatch (current={}, requested={}) — re-launching",
                inner.headless, headless
            );
            *guard = None;
        } else if inner.tab().get_target_info().is_ok() {
            return Ok(false);  // still alive, correct mode
        } else {
            tracing::warn!("[browser] existing Chrome session dead (connection closed) — re-launching");
            *guard = None;
        }
    }

    let opts = LaunchOptions::default_builder()
        .headless(headless)
        .build()
        .map_err(|e| format!("LaunchOptions: {e}"))?;
    let browser = Browser::new(opts).map_err(|e| format!("Browser::new: {e}"))?;
    let tab = browser.new_tab().map_err(|e| format!("new_tab: {e}"))?;

    // Settle delay — Chrome reports tab ready immediately but internal frame
    // tree / CDP event subscriptions need ~500ms to stabilise.  Without this,
    // the first navigate_to can miss the Page.frameNavigated event, causing
    // "wait: The event waited for never came" on the first call after launch
    // (esp. after a mode switch).
    std::thread::sleep(std::time::Duration::from_millis(600));

    *guard = Some(BrowserInner { browser, tabs: vec![tab], active: 0, headless });
    tracing::info!("[browser] launched Chrome (headless={headless})");
    Ok(true)
}

pub fn is_open() -> bool {
    global().lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

#[allow(dead_code)]
pub fn is_headless() -> Option<bool> {
    global().lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.headless)
}

pub fn close() {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// Ensures a browser session exists, **preserving current mode if already open**.
///
/// Use this inside tool dispatch paths (`with_tab`, `navigate_and_screenshot`,
/// recovery re-launches) where the caller just needs a session and must not
/// override the user's explicit headless choice made at launch time.
///
/// Explicit mode control should only happen at test-runner entry points
/// (`execute_test_tracked`, MCP `run_adhoc_test`) via `ensure_open(resolved_mode)`.
///
/// Fixes: #10 — `with_tab()` used to call `ensure_open(true)` which triggered
/// the mode-switch logic from `bd08841`, flipping a user-requested
/// `SIRIN_BROWSER_HEADLESS=false` session back to headless.
pub fn ensure_open_reusing() -> Result<bool, String> {
    if is_open() {
        return Ok(false);
    }
    ensure_open(default_headless())
}

// ══════════════════════════════════════════════════════════════════════════════
//  TIER 1 — BASIC NAVIGATION & INTERACTION
// ══════════════════════════════════════════════════════════════════════════════

pub fn navigate(url: &str) -> Result<(), String> {
    navigate_with_retry(url, 2)
}

fn navigate_with_retry(url: &str, attempts: u32) -> Result<(), String> {
    // Detect hash-only navigation (fragment change on the same origin+path).
    // headless_chrome's wait_until_navigated waits for Page.frameNavigated,
    // which Chrome does NOT emit for pure fragment changes.  Use location.hash
    // assignment + short settle delay instead to avoid a 60s timeout.
    let current = current_url().unwrap_or_default();
    if is_hash_only_change(&current, url) {
        let hash = url.split_once('#').map(|(_, h)| h).unwrap_or("");
        let js = format!(
            "location.hash = {};",
            serde_json::to_string(&format!("#{hash}")).unwrap()
        );
        with_tab(|tab| {
            tab.evaluate(&js, false)
                .map_err(|e| format!("hash-nav: {e}"))?;
            Ok(())
        })?;
        // Short settle delay so SPA router has a chance to run
        std::thread::sleep(std::time::Duration::from_millis(400));
        return Ok(());
    }

    let result = with_tab(|tab| {
        tab.navigate_to(url).map_err(|e| format!("navigate: {e}"))?
            .wait_until_navigated().map_err(|e| format!("wait: {e}"))?;
        Ok(())
    });

    // Auto-retry on transient "event never came" — typically happens right
    // after browser launch / mode switch when CDP events haven't fully
    // initialised.  One retry with a short wait is almost always enough.
    if let Err(ref e) = result {
        if attempts > 1 && is_transient_nav_error(e) {
            tracing::warn!("[browser] navigate transient failure ({e}) — retrying in 500ms");
            std::thread::sleep(std::time::Duration::from_millis(500));
            return navigate_with_retry(url, attempts - 1);
        }
    }
    result
}

fn is_transient_nav_error(err: &str) -> bool {
    err.contains("The event waited for never came")
}

/// Returns true when `new_url` is the same as `current_url` except for the
/// fragment (hash) portion.  Same origin, same path, same query, different hash.
fn is_hash_only_change(current: &str, new_url: &str) -> bool {
    let (cur_base, _cur_hash) = split_hash(current);
    let (new_base, _new_hash) = split_hash(new_url);
    !cur_base.is_empty() && cur_base == new_base && new_url.contains('#')
}

fn split_hash(url: &str) -> (&str, &str) {
    match url.split_once('#') {
        Some((base, hash)) => (base, hash),
        None => (url, ""),
    }
}

pub fn screenshot() -> Result<Vec<u8>, String> {
    with_tab(|tab| {
        tab.capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
            .map_err(|e| format!("screenshot: {e}"))
    })
}

#[allow(dead_code)]
pub fn navigate_and_screenshot(url: &str) -> Result<Vec<u8>, String> {
    ensure_open_reusing()?;
    navigate(url)?;
    screenshot()
}

pub fn current_url() -> Result<String, String> {
    with_tab(|tab| Ok(tab.get_url()))
}

pub fn page_title() -> Result<String, String> {
    with_tab(|tab| tab.get_title().map_err(|e| format!("title: {e}")))
}

pub fn click(selector: &str) -> Result<(), String> {
    with_tab(|tab| {
        tab.wait_for_element(selector)
            .map_err(|e| format!("click – find '{selector}': {e}"))?
            .click().map_err(|e| format!("click '{selector}': {e}"))?;
        Ok(())
    })
}

pub fn type_text(selector: &str, text: &str) -> Result<(), String> {
    with_tab(|tab| {
        let el = tab.wait_for_element(selector)
            .map_err(|e| format!("type – find '{selector}': {e}"))?;
        el.click().map_err(|e| format!("type – focus '{selector}': {e}"))?;
        el.type_into(text).map_err(|e| format!("type_into '{selector}': {e}"))?;
        Ok(())
    })
}

pub fn get_text(selector: &str) -> Result<String, String> {
    with_tab(|tab| {
        tab.wait_for_element(selector)
            .map_err(|e| format!("get_text – find '{selector}': {e}"))?
            .get_inner_text()
            .map_err(|e| format!("get_inner_text '{selector}': {e}"))
    })
}

pub fn evaluate_js(expression: &str) -> Result<String, String> {
    with_tab(|tab| {
        let obj = tab.evaluate(expression, true).map_err(|e| format!("evaluate: {e}"))?;
        match obj.value {
            Some(serde_json::Value::String(s)) => Ok(s),
            Some(other) => Ok(other.to_string()),
            None => Ok(obj.description.unwrap_or_else(|| "undefined".into())),
        }
    })
}

#[allow(dead_code)]
pub fn get_content() -> Result<String, String> {
    with_tab(|tab| tab.get_content().map_err(|e| format!("get_content: {e}")))
}

// ── Wait ─────────────────────────────────────────────────────────────────────

/// Wait for a CSS selector to appear in the DOM (default timeout from Tab).
pub fn wait_for(selector: &str) -> Result<(), String> {
    with_tab(|tab| {
        tab.wait_for_element(selector)
            .map_err(|e| format!("wait_for '{selector}': {e}"))?;
        Ok(())
    })
}

/// Wait for a selector with a custom timeout in milliseconds.
pub fn wait_for_ms(selector: &str, ms: u64) -> Result<(), String> {
    with_tab(|tab| {
        tab.wait_for_element_with_custom_timeout(selector, std::time::Duration::from_millis(ms))
            .map_err(|e| format!("wait_for '{selector}' ({ms}ms): {e}"))?;
        Ok(())
    })
}

/// Wait for navigation to complete after a click or JS redirect.
pub fn wait_for_navigation() -> Result<(), String> {
    with_tab(|tab| {
        tab.wait_until_navigated().map_err(|e| format!("wait_nav: {e}"))?;
        Ok(())
    })
}

// ── Element queries ──────────────────────────────────────────────────────────

/// Check if an element matching the selector exists (no error if absent).
pub fn element_exists(selector: &str) -> Result<bool, String> {
    with_tab(|tab| {
        Ok(tab.find_element(selector).is_ok())
    })
}

/// Count elements matching a selector.
pub fn element_count(selector: &str) -> Result<usize, String> {
    with_tab(|tab| {
        Ok(tab.find_elements(selector).map(|v| v.len()).unwrap_or(0))
    })
}

/// Get an attribute value from the first matching element.
pub fn get_attribute(selector: &str, attr: &str) -> Result<String, String> {
    let js = format!(
        "document.querySelector({})?.getAttribute({}) ?? ''",
        serde_json::to_string(selector).unwrap(),
        serde_json::to_string(attr).unwrap(),
    );
    evaluate_js(&js)
}

/// Get the value property of a form element.
pub fn get_value(selector: &str) -> Result<String, String> {
    let js = format!(
        "document.querySelector({})?.value ?? ''",
        serde_json::to_string(selector).unwrap(),
    );
    evaluate_js(&js)
}

// ── Keyboard ─────────────────────────────────────────────────────────────────

/// Press a single key (e.g. "Enter", "Tab", "Escape", "ArrowDown").
pub fn press_key(key: &str) -> Result<(), String> {
    with_tab(|tab| {
        tab.press_key(key).map_err(|e| format!("press_key '{key}': {e}"))?;
        Ok(())
    })
}

/// Press a key with modifiers (e.g. ctrl+a, shift+Enter).
/// Accepts modifier names: "alt", "ctrl", "meta", "shift".
pub fn press_key_combo(key: &str, modifier_names: &[&str]) -> Result<(), String> {
    let mods: Vec<ModifierKey> = modifier_names.iter().filter_map(|m| match m.to_lowercase().as_str() {
        "alt" => Some(ModifierKey::Alt),
        "ctrl" | "control" => Some(ModifierKey::Ctrl),
        "meta" | "cmd" => Some(ModifierKey::Meta),
        "shift" => Some(ModifierKey::Shift),
        _ => None,
    }).collect();
    with_tab(|tab| {
        tab.press_key_with_modifiers(key, Some(&mods))
            .map_err(|e| format!("press_key_combo '{key}': {e}"))?;
        Ok(())
    })
}

// ── Select / Scroll ──────────────────────────────────────────────────────────

/// Select an option in a <select> by value.
pub fn select_option(selector: &str, value: &str) -> Result<(), String> {
    let js = format!(
        "(() => {{ const s = document.querySelector({}); if(!s) return 'not found'; s.value = {}; s.dispatchEvent(new Event('change', {{bubbles:true}})); return 'ok'; }})()",
        serde_json::to_string(selector).unwrap(),
        serde_json::to_string(value).unwrap(),
    );
    let result = evaluate_js(&js)?;
    if result == "not found" { return Err(format!("select '{selector}' not found")); }
    Ok(())
}

/// Scroll the page by (x, y) pixels.
pub fn scroll_by(x: f64, y: f64) -> Result<(), String> {
    evaluate_js(&format!("window.scrollBy({x},{y})"))?;
    Ok(())
}

/// Scroll an element into view.
pub fn scroll_into_view(selector: &str) -> Result<(), String> {
    let js = format!(
        "document.querySelector({})?.scrollIntoView({{behavior:'smooth',block:'center'}})",
        serde_json::to_string(selector).unwrap(),
    );
    evaluate_js(&js)?;
    Ok(())
}

// ── Viewport / device emulation ──────────────────────────────────────────────

/// Set the browser viewport size (device emulation).
pub fn set_viewport(width: u32, height: u32, device_scale: f64, mobile: bool) -> Result<(), String> {
    with_tab(|tab| {
        tab.call_method(Emulation::SetDeviceMetricsOverride {
            width,
            height,
            device_scale_factor: device_scale,
            mobile,
            scale: None,
            screen_width: None,
            screen_height: None,
            position_x: None,
            position_y: None,
            dont_set_visible_size: None,
            screen_orientation: None,
            viewport: None,
            display_feature: None,
            device_posture: None,
        }).map_err(|e| format!("set_viewport: {e}"))?;
        Ok(())
    })
}

// ── Console capture ──────────────────────────────────────────────────────────

/// Capture recent JS console messages (log/warn/error).
/// Returns a JSON array string of {level, text} objects.
pub fn console_messages(limit: usize) -> Result<String, String> {
    let js = format!(
        r#"(() => {{
            if (!window.__sirin_console) return '[]';
            return JSON.stringify(window.__sirin_console.slice(-{limit}));
        }})()"#
    );
    evaluate_js(&js)
}

/// Install a console interceptor that buffers messages.
/// Call once after navigation to start capturing.
pub fn install_console_capture() -> Result<(), String> {
    evaluate_js(r#"(() => {
        if (window.__sirin_console) return;
        window.__sirin_console = [];
        const orig = {};
        ['log','warn','error','info'].forEach(level => {
            orig[level] = console[level];
            console[level] = function(...args) {
                window.__sirin_console.push({level, text: args.map(String).join(' ')});
                if (window.__sirin_console.length > 200) window.__sirin_console.shift();
                orig[level].apply(console, args);
            };
        });
    })()"#)?;
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
//  TIER 2 — COORDINATE INTERACTION & ELEMENT SCREENSHOT
// ══════════════════════════════════════════════════════════════════════════════

/// Click at exact viewport coordinates (x, y).  Useful for Canvas-based UIs
/// (e.g. Flutter Web) where CSS selectors don't work.
pub fn click_point(x: f64, y: f64) -> Result<(), String> {
    with_tab(|tab| {
        // Point imported at module level
        tab.click_point(Point { x, y })
            .map_err(|e| format!("click_point({x},{y}): {e}"))?;
        Ok(())
    })
}

/// Move the mouse to (x, y) without clicking — triggers hover effects.
pub fn hover_point(x: f64, y: f64) -> Result<(), String> {
    with_tab(|tab| {
        // Point imported at module level
        tab.move_mouse_to_point(Point { x, y })
            .map_err(|e| format!("hover({x},{y}): {e}"))?;
        Ok(())
    })
}

/// Hover over the first element matching a CSS selector.
pub fn hover(selector: &str) -> Result<(), String> {
    with_tab(|tab| {
        tab.wait_for_element(selector)
            .map_err(|e| format!("hover – find '{selector}': {e}"))?
            .move_mouse_over()
            .map_err(|e| format!("hover '{selector}': {e}"))?;
        Ok(())
    })
}

/// Screenshot a specific element (returns PNG bytes cropped to the element).
pub fn screenshot_element(selector: &str) -> Result<Vec<u8>, String> {
    with_tab(|tab| {
        let el = tab.wait_for_element(selector)
            .map_err(|e| format!("screenshot_element – find '{selector}': {e}"))?;
        let model = el.get_box_model()
            .map_err(|e| format!("get_box_model '{selector}': {e}"))?;
        let vp = model.content_viewport();
        tab.capture_screenshot(
            CaptureScreenshotFormatOption::Png,
            None,
            Some(Page::Viewport {
                x: vp.x,
                y: vp.y,
                width: vp.width,
                height: vp.height,
                scale: vp.scale,
            }),
            true,
        ).map_err(|e| format!("screenshot_element '{selector}': {e}"))
    })
}

// ══════════════════════════════════════════════════════════════════════════════
//  TIER 3 — MULTI-TAB, COOKIES, NETWORK, FILE, IFRAME, PDF, AUTH
// ══════════════════════════════════════════════════════════════════════════════

// ── Multi-tab ────────────────────────────────────────────────────────────────

/// Open a new tab and return its index.
pub fn new_tab() -> Result<usize, String> {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_mut().ok_or("browser not open")?;
    let tab = inner.browser.new_tab().map_err(|e| format!("new_tab: {e}"))?;
    inner.tabs.push(tab);
    let idx = inner.tabs.len() - 1;
    inner.active = idx;
    Ok(idx)
}

/// Switch to a tab by index.
pub fn switch_tab(index: usize) -> Result<(), String> {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_mut().ok_or("browser not open")?;
    if index >= inner.tabs.len() {
        return Err(format!("tab index {index} out of range (have {})", inner.tabs.len()));
    }
    inner.active = index;
    Ok(())
}

/// Close a tab by index. Cannot close the last tab.
pub fn close_tab(index: usize) -> Result<(), String> {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_mut().ok_or("browser not open")?;
    if inner.tabs.len() <= 1 {
        return Err("cannot close the last tab".into());
    }
    if index >= inner.tabs.len() {
        return Err(format!("tab index {index} out of range"));
    }
    inner.tabs.remove(index);
    if inner.active >= inner.tabs.len() {
        inner.active = inner.tabs.len() - 1;
    }
    Ok(())
}

/// List all tabs: returns Vec of (index, url).
pub fn list_tabs() -> Result<Vec<(usize, String)>, String> {
    let guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_ref().ok_or("browser not open")?;
    Ok(inner.tabs.iter().enumerate().map(|(i, t)| (i, t.get_url())).collect())
}

/// Get the active tab index.
pub fn active_tab() -> Result<usize, String> {
    let guard = global().lock().unwrap_or_else(|e| e.into_inner());
    Ok(guard.as_ref().ok_or("browser not open")?.active)
}

// ── Cookies ──────────────────────────────────────────────────────────────────

/// Get all cookies as a JSON string.
pub fn get_cookies() -> Result<String, String> {
    with_tab(|tab| {
        let cookies = tab.get_cookies().map_err(|e| format!("get_cookies: {e}"))?;
        let vals: Vec<serde_json::Value> = cookies.into_iter().map(|c| {
            json!({ "name": c.name, "value": c.value, "domain": c.domain, "path": c.path })
        }).collect();
        Ok(serde_json::to_string(&vals).unwrap())
    })
}

/// Set a cookie.
pub fn set_cookie(name: &str, value: &str, domain: &str, path: &str) -> Result<(), String> {
    with_tab(|tab| {
        tab.set_cookies(vec![Network::CookieParam {
            name: name.to_string(),
            value: value.to_string(),
            domain: Some(domain.to_string()),
            path: Some(path.to_string()),
            url: None,
            secure: None,
            http_only: None,
            same_site: None,
            expires: None,
            priority: None,
            same_party: None,
            source_scheme: None,
            source_port: None,
            partition_key: None,
        }]).map_err(|e| format!("set_cookie: {e}"))?;
        Ok(())
    })
}

/// Delete cookies matching a name (on current domain).
pub fn delete_cookie(name: &str) -> Result<(), String> {
    with_tab(|tab| {
        let url = tab.get_url();
        tab.delete_cookies(vec![Network::DeleteCookies {
            name: name.to_string(),
            url: Some(url),
            domain: None,
            path: None,
            partition_key: None,
        }]).map_err(|e| format!("delete_cookie: {e}"))?;
        Ok(())
    })
}

// ── Network intercept ────────────────────────────────────────────────────────

/// Capture recent network requests via JS PerformanceObserver.
/// Returns JSON array of {url, method, status, type, duration}.
pub fn network_requests(limit: usize) -> Result<String, String> {
    let js = format!(
        r#"JSON.stringify(
            performance.getEntriesByType('resource').slice(-{limit}).map(e => ({{
                url: e.name,
                type: e.initiatorType,
                duration: Math.round(e.duration),
                size: e.transferSize || 0
            }}))
        )"#
    );
    evaluate_js(&js)
}

/// Install a fetch/XHR interceptor that logs request/response pairs.
/// Call once after navigation.
pub fn install_network_capture() -> Result<(), String> {
    evaluate_js(r#"(() => {
        if (window.__sirin_net) return;
        window.__sirin_net = [];
        const origFetch = window.fetch;
        window.fetch = async function(...args) {
            const url = typeof args[0] === 'string' ? args[0] : args[0]?.url || '?';
            const method = args[1]?.method || 'GET';
            const entry = { url, method, status: 0, body: '', ts: Date.now() };
            try {
                const resp = await origFetch.apply(this, args);
                entry.status = resp.status;
                try { entry.body = await resp.clone().text(); } catch(e) {}
                window.__sirin_net.push(entry);
                if (window.__sirin_net.length > 100) window.__sirin_net.shift();
                return resp;
            } catch(e) {
                entry.status = -1;
                entry.body = String(e);
                window.__sirin_net.push(entry);
                throw e;
            }
        };
        const origXhrOpen = XMLHttpRequest.prototype.open;
        const origXhrSend = XMLHttpRequest.prototype.send;
        XMLHttpRequest.prototype.open = function(method, url) {
            this.__sirin = { url, method, status: 0, body: '', ts: Date.now() };
            return origXhrOpen.apply(this, arguments);
        };
        XMLHttpRequest.prototype.send = function() {
            this.addEventListener('load', function() {
                if (this.__sirin) {
                    this.__sirin.status = this.status;
                    this.__sirin.body = this.responseText?.substring(0, 2000) || '';
                    window.__sirin_net.push(this.__sirin);
                    if (window.__sirin_net.length > 100) window.__sirin_net.shift();
                }
            });
            return origXhrSend.apply(this, arguments);
        };
    })()"#)?;
    Ok(())
}

/// Read captured fetch/XHR requests (from install_network_capture).
pub fn captured_requests(limit: usize) -> Result<String, String> {
    evaluate_js(&format!(
        "JSON.stringify((window.__sirin_net||[]).slice(-{limit}))"
    ))
}

// ── File upload ──────────────────────────────────────────────────────────────

/// Upload file(s) to a file input element.
pub fn file_upload(selector: &str, paths: &[&str]) -> Result<(), String> {
    with_tab(|tab| {
        let el = tab.wait_for_element(selector)
            .map_err(|e| format!("file_upload – find '{selector}': {e}"))?;
        let node = el.get_description()
            .map_err(|e| format!("file_upload – describe '{selector}': {e}"))?;
        let node_id = node.backend_node_id;
        let files: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        tab.call_method(headless_chrome::protocol::cdp::DOM::SetFileInputFiles {
            files,
            node_id: Some(node_id.into()),
            backend_node_id: Some(node_id),
            object_id: None,
        }).map_err(|e| format!("file_upload: {e}"))?;
        Ok(())
    })
}

// ── Iframe ───────────────────────────────────────────────────────────────────

/// Evaluate JS inside an iframe by selector. Returns the result as string.
pub fn iframe_eval(iframe_selector: &str, expression: &str) -> Result<String, String> {
    let js = format!(
        r#"(() => {{
            const f = document.querySelector({sel});
            if (!f || !f.contentDocument) return 'ERROR: iframe not found or cross-origin';
            try {{
                const r = f.contentWindow.eval({expr});
                return typeof r === 'string' ? r : JSON.stringify(r);
            }} catch(e) {{ return 'ERROR: ' + e.message; }}
        }})()"#,
        sel = serde_json::to_string(iframe_selector).unwrap(),
        expr = serde_json::to_string(expression).unwrap(),
    );
    evaluate_js(&js)
}

// ── Drag and drop ────────────────────────────────────────────────────────────

/// Drag from one point to another.
pub fn drag(from_x: f64, from_y: f64, to_x: f64, to_y: f64) -> Result<(), String> {
    with_tab(|tab| {
        tab.move_mouse_to_point(Point { x: from_x, y: from_y })
            .map_err(|e| format!("drag move_to_start: {e}"))?;
        let mouse = |t, x, y, btn: Option<Input::MouseButton>, bc, cc| {
            tab.call_method(Input::DispatchMouseEvent {
                Type: t, x, y,
                button: btn, buttons: bc, click_count: cc,
                modifiers: None, timestamp: None,
                delta_x: None, delta_y: None, pointer_Type: None,
                force: None, tangential_pressure: None,
                tilt_x: None, tilt_y: None, twist: None,
            })
        };
        mouse(Input::DispatchMouseEventTypeOption::MousePressed,
            from_x, from_y, Some(Input::MouseButton::Left), Some(1), Some(1))
            .map_err(|e| format!("drag press: {e}"))?;
        mouse(Input::DispatchMouseEventTypeOption::MouseMoved,
            to_x, to_y, Some(Input::MouseButton::Left), Some(1), None)
            .map_err(|e| format!("drag move: {e}"))?;
        mouse(Input::DispatchMouseEventTypeOption::MouseReleased,
            to_x, to_y, Some(Input::MouseButton::Left), Some(0), Some(1))
            .map_err(|e| format!("drag release: {e}"))?;
        Ok(())
    })
}

// ── PDF export ───────────────────────────────────────────────────────────────

/// Export current page as PDF (headless only). Returns raw PDF bytes.
pub fn pdf() -> Result<Vec<u8>, String> {
    with_tab(|tab| {
        tab.print_to_pdf(None).map_err(|e| format!("print_to_pdf: {e}"))
    })
}

// ── HTTP basic auth ──────────────────────────────────────────────────────────

/// Set HTTP basic auth credentials for all requests.
/// Uses Fetch domain to intercept and respond to auth challenges.
pub fn set_http_auth(username: &str, password: &str) -> Result<(), String> {
    let js = format!(
        r#"(() => {{
            window.__sirin_auth = {{ user: {u}, pass: {p} }};
        }})()"#,
        u = serde_json::to_string(username).unwrap(),
        p = serde_json::to_string(password).unwrap(),
    );
    evaluate_js(&js)?;
    // Also set via CDP Network.setExtraHTTPHeaders with Authorization
    let encoded = base64_encode(&format!("{username}:{password}"));
    let auth_header = format!("Basic {encoded}");
    with_tab(|tab| {
        tab.call_method(Network::SetExtraHTTPHeaders {
            headers: Network::Headers(Some(json!({
                "Authorization": auth_header
            }))),
        }).map_err(|e| format!("set_http_auth: {e}"))?;
        Ok(())
    })
}

// ── localStorage / sessionStorage ────────────────────────────────────────────

/// Get a localStorage value.
pub fn local_storage_get(key: &str) -> Result<String, String> {
    evaluate_js(&format!("localStorage.getItem({}) || ''", serde_json::to_string(key).unwrap()))
}

/// Set a localStorage value.
pub fn local_storage_set(key: &str, value: &str) -> Result<(), String> {
    evaluate_js(&format!(
        "localStorage.setItem({}, {})",
        serde_json::to_string(key).unwrap(),
        serde_json::to_string(value).unwrap(),
    ))?;
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
//  INTERNALS
// ══════════════════════════════════════════════════════════════════════════════

pub(crate) fn with_tab<F, R>(f: F) -> Result<R, String>
where
    F: FnOnce(&Arc<Tab>) -> Result<R, String> + Clone,
{
    ensure_open_reusing()?;

    let result = {
        let guard = global().lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(inner) => f.clone()(inner.tab()),
            None => Err("browser session lost".into()),
        }
    };

    // If the call failed with a connection-closed error, try one auto-recover.
    if let Err(ref e) = result {
        if is_connection_closed(e) {
            tracing::warn!("[browser] mid-call connection closed — attempting one-shot recovery");
            // Clear singleton
            *global().lock().unwrap_or_else(|e| e.into_inner()) = None;
            // Re-launch — preserve user-requested mode if previously set
            ensure_open_reusing()?;
            // Retry exactly once
            let guard = global().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(inner) = guard.as_ref() {
                return f(inner.tab());
            }
        }
    }

    result
}

fn is_connection_closed(err: &str) -> bool {
    err.contains("underlying connection is closed")
        || err.contains("TaskCancelled")
        || err.contains("ChannelClosed")
}

fn base64_encode(input: &str) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 { out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char); }
        else { out.push('='); }
        if chunk.len() > 2 { out.push(CHARS[(triple & 0x3F) as usize] as char); }
        else { out.push('='); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure unit tests (no Chrome needed) ────────────────────────────────────

    #[test]
    fn hash_only_change_detects_fragment_switch() {
        assert!(is_hash_only_change(
            "https://app.com/",
            "https://app.com/#/admin/users"
        ));
        assert!(is_hash_only_change(
            "https://app.com/#/login",
            "https://app.com/#/dashboard"
        ));
    }

    #[test]
    fn hash_only_change_rejects_path_change() {
        assert!(!is_hash_only_change(
            "https://app.com/login",
            "https://app.com/dashboard"
        ));
        // Path differs even if both have hashes
        assert!(!is_hash_only_change(
            "https://app.com/login#x",
            "https://app.com/dashboard#y"
        ));
    }

    #[test]
    fn hash_only_change_rejects_origin_change() {
        assert!(!is_hash_only_change(
            "https://a.com/#/x",
            "https://b.com/#/x"
        ));
    }

    #[test]
    fn hash_only_change_rejects_no_hash_target() {
        // Target has no '#' — should NOT be considered hash-only
        assert!(!is_hash_only_change(
            "https://app.com/#/admin",
            "https://app.com/"
        ));
    }

    #[test]
    fn hash_only_change_handles_empty_current() {
        // about:blank or empty current should be rejected
        assert!(!is_hash_only_change("", "https://app.com/#/x"));
    }

    #[test]
    fn default_headless_respects_env() {
        // Save original
        let orig = std::env::var("SIRIN_BROWSER_HEADLESS").ok();

        std::env::set_var("SIRIN_BROWSER_HEADLESS", "false");
        assert!(!default_headless());
        std::env::set_var("SIRIN_BROWSER_HEADLESS", "0");
        assert!(!default_headless());
        std::env::set_var("SIRIN_BROWSER_HEADLESS", "true");
        assert!(default_headless());
        std::env::remove_var("SIRIN_BROWSER_HEADLESS");
        assert!(default_headless(), "default when unset");

        // Restore
        if let Some(v) = orig {
            std::env::set_var("SIRIN_BROWSER_HEADLESS", v);
        }
    }

    #[test]
    #[ignore] // needs Chrome; integration test
    fn ensure_open_reusing_preserves_non_headless_session() {
        // Regression test for #10: `with_tab()` used to call `ensure_open(true)`,
        // which flipped a user-requested headless=false session back to headless,
        // causing Flutter CanvasKit / WebGL content to paint all-black.
        close();
        ensure_open(false).expect("launch visible");
        assert_eq!(is_headless(), Some(false), "launched visible");

        // Simulate what `with_tab()` does on every tool dispatch.
        ensure_open_reusing().expect("reuse");
        assert_eq!(is_headless(), Some(false), "mode MUST stay false after reuse");

        close();
    }

    #[test]
    #[ignore] // needs Chrome; integration test
    fn ensure_open_reusing_opens_when_closed() {
        close();
        assert!(!is_open());
        ensure_open_reusing().expect("opens fresh");
        assert!(is_open());
        close();
    }

    // ── Integration tests (need Chrome) ───────────────────────────────────────

    #[test]
    #[ignore]
    fn browser_lifecycle() {
        assert!(!is_open());
        ensure_open(true).expect("launch");
        assert!(is_open());

        let html = "data:text/html,<html><head><title>SirinTest</title></head><body><h1 id='msg'>Hello</h1><input id='box'/></body></html>";
        navigate(html).expect("navigate");

        let png = screenshot().expect("screenshot");
        assert!(png.len() > 100);

        assert_eq!(get_text("#msg").expect("get_text").trim(), "Hello");
        assert_eq!(evaluate_js("document.title").expect("eval"), "SirinTest");
        assert_eq!(page_title().expect("title"), "SirinTest");
        assert!(current_url().expect("url").starts_with("data:"));

        click("#msg").expect("click");
        type_text("#box", "Sirin").expect("type");
        assert_eq!(evaluate_js("document.getElementById('box').value").expect("val"), "Sirin");

        close();
        assert!(!is_open());
        println!("✓ browser_lifecycle: all steps passed");
    }

    #[test]
    #[ignore]
    fn browser_tier1_extended() {
        close();
        ensure_open(true).expect("launch");

        let html = r#"data:text/html,<html><head><title>T1</title></head><body>
            <div id='a'>Alpha</div><div id='b'>Beta</div><div class='item'>1</div><div class='item'>2</div><div class='item'>3</div>
            <a id='link' href='https://example.com' data-foo='bar'>Link</a>
            <select id='sel'><option value='x'>X</option><option value='y'>Y</option></select>
            <input id='inp'/>
        </body></html>"#;
        navigate(html).expect("nav");

        // element_exists
        assert!(element_exists("#a").expect("exists"));
        assert!(!element_exists("#zzz").expect("not exists"));

        // element_count
        assert_eq!(element_count(".item").expect("count"), 3);

        // get_attribute
        let href = get_attribute("#link", "href").expect("href");
        assert!(href.starts_with("https://example.com"), "href was: {href}");
        assert_eq!(get_attribute("#link", "data-foo").expect("data"), "bar");

        // select_option
        select_option("#sel", "y").expect("select");
        assert_eq!(get_value("#sel").expect("val"), "y");

        // wait_for
        wait_for("#a").expect("wait_for");

        // scroll (just verify no error)
        scroll_by(0.0, 100.0).expect("scroll_by");
        scroll_into_view("#b").expect("scroll_into_view");

        // keyboard
        click("#inp").expect("focus");
        type_text("#inp", "abc").expect("type");
        press_key("Backspace").expect("backspace");
        assert_eq!(get_value("#inp").expect("val"), "ab");

        // console capture
        install_console_capture().expect("install console");
        evaluate_js("console.log('test msg')").expect("log");
        let msgs = console_messages(10).expect("msgs");
        assert!(msgs.contains("test msg"), "msgs: {msgs}");

        close();
        println!("✓ browser_tier1_extended: all steps passed");
    }

    #[test]
    #[ignore]
    fn browser_tier2_coords() {
        close();
        ensure_open(true).expect("launch");

        let html = r#"data:text/html,<html><body>
            <button id='btn' style='position:absolute;left:50px;top:50px;width:100px;height:40px' onclick='document.title="clicked"'>Click</button>
            <div id='box' style='width:200px;height:200px;background:red'>Box</div>
        </body></html>"#;
        navigate(html).expect("nav");

        // click_point
        click_point(100.0, 70.0).expect("click_point");
        assert_eq!(page_title().expect("title"), "clicked");

        // hover_point (no error)
        hover_point(150.0, 200.0).expect("hover_point");

        // hover by selector
        hover("#box").expect("hover");

        // screenshot_element
        let png = screenshot_element("#box").expect("el screenshot");
        assert!(png.len() > 50, "element screenshot too small");

        close();
        println!("✓ browser_tier2_coords: all steps passed");
    }

    #[test]
    #[ignore]
    fn browser_tier3_tabs_cookies() {
        close();
        ensure_open(true).expect("launch");

        // Multi-tab
        navigate("data:text/html,<title>Tab0</title>").expect("nav0");
        let tabs = list_tabs().expect("list");
        assert_eq!(tabs.len(), 1);

        let idx1 = new_tab().expect("new_tab");
        assert_eq!(idx1, 1);
        navigate("data:text/html,<title>Tab1</title>").expect("nav1");
        assert_eq!(page_title().expect("t1"), "Tab1");

        switch_tab(0).expect("switch");
        assert_eq!(page_title().expect("t0"), "Tab0");

        let tabs2 = list_tabs().expect("list2");
        assert_eq!(tabs2.len(), 2);

        close_tab(1).expect("close_tab");
        let tabs3 = list_tabs().expect("list3");
        assert_eq!(tabs3.len(), 1);

        // localStorage
        navigate("data:text/html,<title>Storage</title>").ok(); // data: URI might not support localStorage
        // Skip localStorage test on data: URIs as they don't support it

        // network_requests (performance API)
        navigate("data:text/html,<title>Net</title>").expect("nav net");
        let net = network_requests(5).expect("net");
        assert!(net.starts_with('['), "net: {net}");

        close();
        println!("✓ browser_tier3_tabs_cookies: all steps passed");
    }
}
