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
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

// ── Companion extension discovery (stub — see DESIGN_BROWSER_AUTHORITY.md) ──
//
// Look for `ext/manifest.json` next to the binary first (installed mode), then
// in CWD (dev mode).  Returns the *directory* path Chrome would `--load-extension=`.
//
// Currently UNUSED at runtime: Chrome 147 ignores `--load-extension` even with
// every opt-out flag, so `launch_with_mode` no longer plumbs this through.
// Kept compiled (with `#[allow(dead_code)]`) so the discovery contract stays
// frozen — the day we ship Chrome for Testing as a sidecar, only the launch
// site needs to change.
#[allow(dead_code)]
fn locate_companion_ext() -> Option<std::path::PathBuf> {
    // Chrome resolves `--load-extension=<path>` relative to its own CWD, NOT
    // ours. Always pass an absolute path; otherwise the extension silently
    // fails to load (no error, no SW, no diagnostic — just nothing).
    let candidates = [
        std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.join("ext"))),
        std::env::current_dir().ok().map(|p| p.join("ext")),
    ];
    for cand in candidates.into_iter().flatten() {
        if cand.join("manifest.json").is_file() {
            // Canonicalize so Chrome gets a stable absolute path. Windows
            // canonicalize returns `\\?\C:\...` (UNC verbatim) which Chrome
            // refuses; strip that prefix.
            let abs = std::fs::canonicalize(&cand).unwrap_or(cand);
            #[cfg(windows)]
            let abs = {
                let s = abs.to_string_lossy();
                if let Some(rest) = s.strip_prefix(r"\\?\") {
                    std::path::PathBuf::from(rest)
                } else {
                    abs
                }
            };
            return Some(abs);
        }
    }
    None
}

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
    /// Named sessions: session_id → tab index.
    /// Allows callers to maintain multiple independent browser contexts
    /// (e.g. buyer_a / buyer_b for cross-role E2E tests).
    sessions: HashMap<String, usize>,
    /// Last viewport set via set_viewport() — (width, height, scale, mobile).
    /// Re-applied automatically after goto/clear_state/new_tab because
    /// CDP Emulation.setDeviceMetricsOverride does NOT persist across
    /// full navigations or new-tab creation (Issue #27).
    viewport: Option<(u32, u32, f64, bool)>,
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

    // Sirin Companion extension auto-load — DISABLED.
    //
    // Original plan: pass `--load-extension=` to push authoritative tab state
    // from chrome.tabs.* / chrome.webNavigation.* into Sirin via WebSocket.
    //
    // Verified 2026-04-18 (Chrome 147): `--load-extension` is a no-op.  Chrome
    // 122 deprecated it; Chrome ~147 removed the opt-out feature flag
    // `DisableLoadExtensionCommandLineSwitch`, so even with every right flag
    // the unpacked extension is silently ignored — no warning, no SW spawn,
    // chrome.developerPrivate.getExtensionsInfo() returns zero unpacked items.
    //
    // We keep `locate_companion_ext()`, `ext/`, and `src/ext_server.rs` in
    // tree as stubs for the day we ship Chrome for Testing alongside Sirin
    // (CfT preserves the legacy command-line behaviour).  The runtime cost
    // of the unused WS endpoint is ~0 — it just never receives a connection.
    //
    // The user-visible problem (#23 stale URL/title) is now solved by reading
    // live page state via `Runtime.evaluate` in `current_url()` /
    // `page_title()` instead of `Tab::get_url()` / `Tab::get_title()`.
    //
    // See: docs/DESIGN_BROWSER_AUTHORITY.md
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

    *guard = Some(BrowserInner { browser, tabs: vec![tab], active: 0, headless, sessions: HashMap::new(), viewport: None });
    // Clear per-tab a11y state so the new session's tab index 0 starts fresh.
    crate::browser_ax::reset_a11y_enabled();
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

    // Re-apply cached viewport after a full navigation (Issue #27):
    // Emulation.setDeviceMetricsOverride resets on page load.
    if result.is_ok() {
        reapply_viewport();
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

/// Capture current tab as JPEG with the given quality (1-100).
/// Prefer over `screenshot()` when streaming (e.g. Monitor view) — JPEG 80
/// compresses Flutter / typical UI screens to ~50 KB vs 500 KB for PNG.
pub fn screenshot_jpeg(quality: u8) -> Result<Vec<u8>, String> {
    with_tab(|tab| {
        let q = quality.clamp(1, 100) as u32;
        tab.capture_screenshot(
            CaptureScreenshotFormatOption::Jpeg,
            Some(q),
            None,
            true,
        )
        .map_err(|e| format!("screenshot_jpeg: {e}"))
    })
}

#[allow(dead_code)]
pub fn navigate_and_screenshot(url: &str) -> Result<Vec<u8>, String> {
    ensure_open_reusing()?;
    navigate(url)?;
    screenshot()
}

/// Live URL of the active tab.  Uses `Runtime.evaluate("window.location.href")`
/// rather than `Tab::get_url()` so we get the rendered page's truth, not
/// headless_chrome's `target_info` cache which goes stale on:
///   - `about:blank` reset (no `Page.frameNavigated` fires)
///   - Cross-origin redirect race (cache lags `Target.targetInfoChanged`)
///   - SPA hash-only navigation (Chrome emits no `frameNavigated`)
///
/// See `docs/DESIGN_BROWSER_AUTHORITY.md` for the postmortem of why we did
/// not solve this with a Chrome companion extension (Chrome 147+ blocks
/// `--load-extension` outright).
///
/// Falls back to the cached `tab.get_url()` if `Runtime.evaluate` fails
/// (no exec context yet, debugger paused) — at worst no-worse-than-before.
pub fn current_url() -> Result<String, String> {
    with_tab(|tab| {
        match tab.evaluate("window.location.href", false) {
            Ok(obj) => match obj.value {
                Some(serde_json::Value::String(s)) => Ok(s),
                _ => Ok(tab.get_url()), // unexpected shape — fall back
            },
            Err(_) => Ok(tab.get_url()), // navigation race / no context
        }
    })
}

/// Live title of the active tab.  Same rationale as `current_url()` —
/// uses `Runtime.evaluate("document.title")` to read the live DOM rather
/// than the `target_info` cache.
pub fn page_title() -> Result<String, String> {
    with_tab(|tab| {
        match tab.evaluate("document.title", false) {
            Ok(obj) => match obj.value {
                Some(serde_json::Value::String(s)) => Ok(s),
                _ => tab.get_title().map_err(|e| format!("title: {e}")),
            },
            Err(_) => tab.get_title().map_err(|e| format!("title: {e}")),
        }
    })
}

/// Snapshot of the running Chrome process — used by `diagnose` MCP tool to give
/// external AI clients enough context to triage a bug report without asking
/// the user follow-up questions.
///
/// Returns `Ok(None)` when no browser is open (a perfectly valid state, not an
/// error from a diagnostic perspective).
pub fn diagnostic_snapshot() -> Result<Option<DiagnosticSnapshot>, String> {
    let guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let Some(inner) = guard.as_ref() else { return Ok(None); };

    // Browser.getVersion is best-effort — a stale/dead transport returns Err
    // and we still want to report `tabs` + `headless`, so we fall back to None.
    let (chrome_version, user_agent) = match inner.browser.get_version() {
        Ok(v) => (Some(v.product), Some(v.user_agent)),
        Err(_) => (None, None),
    };

    Ok(Some(DiagnosticSnapshot {
        chrome_version,
        user_agent,
        headless: inner.headless,
        active_tab_index: inner.active,
        tab_count: inner.tabs.len(),
        named_sessions: inner.sessions.keys().cloned().collect(),
    }))
}

/// Lightweight diagnostic snapshot — see [`diagnostic_snapshot`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiagnosticSnapshot {
    /// e.g. "Chrome/124.0.6367.92" — None if `Browser.getVersion` failed.
    pub chrome_version: Option<String>,
    pub user_agent:     Option<String>,
    pub headless:       bool,
    pub active_tab_index: usize,
    pub tab_count:      usize,
    pub named_sessions: Vec<String>,
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
/// The dimensions are cached in the session state and automatically
/// re-applied after `navigate`, `clear_browser_state`, and `wait_for_new_tab`
/// because CDP `Emulation.setDeviceMetricsOverride` does not persist across
/// full navigations or new-tab creation (Issue #27).
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
    })?;
    // Cache for automatic re-application after navigation / new-tab.
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(inner) = guard.as_mut() {
        inner.viewport = Some((width, height, device_scale, mobile));
    }
    Ok(())
}

/// Re-apply the last cached viewport to the active tab.
/// No-op when no viewport has been set yet.
/// Errors are logged as warnings but not propagated — caller is not expected
/// to handle a viewport re-apply failure as fatal.
fn reapply_viewport() {
    // Read cache first, release lock before the CDP call (which re-acquires it).
    let cached = {
        let guard = global().lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().and_then(|i| i.viewport)
    };
    if let Some((w, h, scale, mobile)) = cached {
        let r = with_tab(|tab| {
            tab.call_method(Emulation::SetDeviceMetricsOverride {
                width: w,
                height: h,
                device_scale_factor: scale,
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
            }).map_err(|e| format!("reapply_viewport: {e}"))?;
            Ok(())
        });
        if let Err(e) = r {
            tracing::warn!("[browser] reapply_viewport failed (non-fatal): {e}");
        }
    }
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

/// Wait for a new tab to open (e.g. from `target="_blank"` click, OAuth popup,
/// `window.open`).  Polls every 200ms until the tab count grows beyond
/// `baseline_count` or `timeout_ms` elapses.
///
/// On success, the new tab is registered into the singleton's internal tab
/// list, becomes the **active** tab, and its index is returned (caller can
/// `switch_tab` back later).
///
/// Use case: OAuth flows that pop a Google/Telegram window — Sirin
/// previously only saw the original tab and missed the popup entirely.
pub fn wait_for_new_tab(baseline_count: Option<usize>, timeout_ms: u64) -> Result<usize, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    // Measure baseline from the SAME source the loop polls (headless_chrome's
    // Browser.get_tabs()), not from our singleton's cached `inner.tabs` —
    // otherwise we may compare apples to oranges and immediately succeed.
    // We also `register_missing_tabs` first so the baseline reflects all
    // tabs Chrome currently knows about (about:blank, etc).
    let baseline = match baseline_count {
        Some(n) => n,
        None => {
            let guard = global().lock().unwrap_or_else(|e| e.into_inner());
            let inner = guard.as_ref().ok_or("browser not open")?;
            inner.browser.register_missing_tabs();
            let tabs_arc = inner.browser.get_tabs().clone();
            let locked = tabs_arc.lock().unwrap_or_else(|e| e.into_inner());
            locked.len()
        }
    };

    loop {
        // Force the underlying Browser to discover tabs created via window.open
        // — without this, headless_chrome only sees tabs it spawned itself.
        {
            let guard = global().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(inner) = guard.as_ref() {
                inner.browser.register_missing_tabs();
            }
        }

        // Snapshot Browser-level tab count (may be > our cached singleton)
        let browser_tabs: Vec<Arc<Tab>> = {
            let guard = global().lock().unwrap_or_else(|e| e.into_inner());
            let inner = guard.as_ref().ok_or("browser not open")?;
            let tabs_arc = inner.browser.get_tabs().clone();
            let locked = tabs_arc.lock().unwrap_or_else(|e| e.into_inner());
            locked.iter().cloned().collect()
        };

        if browser_tabs.len() > baseline {
            // Find the new tab(s) and adopt them into our singleton.
            let tab_idx = {
                let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
                let inner = guard.as_mut().ok_or("browser not open")?;
                // Add any browser tab not already in inner.tabs.
                let existing_ids: std::collections::HashSet<String> = inner.tabs.iter()
                    .map(|t| t.get_target_id().to_string())
                    .collect();
                for t in &browser_tabs {
                    if !existing_ids.contains(&t.get_target_id().to_string()) {
                        inner.tabs.push(t.clone());
                    }
                }
                // Switch active to the most recent (newest is last in browser_tabs).
                inner.active = inner.tabs.len() - 1;
                inner.active
            };
            // New tabs don't inherit Emulation.setDeviceMetricsOverride from the
            // parent tab — re-apply the cached viewport if one was set (Issue #27).
            reapply_viewport();
            return Ok(tab_idx);
        }

        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "wait_for_new_tab: no new tab within {timeout_ms}ms (count still {})",
                browser_tabs.len()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
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
    // Captures both request body (req_body) and response body (body) for fetch + XHR.
    // Request body matters for K14-style "did the user actually send amount=99.30"
    // assertions — without it, you can only see the response.
    evaluate_js(r#"(() => {
        if (window.__sirin_net) return;
        window.__sirin_net = [];
        const reqBodyToString = (b) => {
            if (b == null) return '';
            if (typeof b === 'string') return b.substring(0, 4000);
            if (b instanceof FormData) {
                try { const o = {}; for (const [k,v] of b.entries()) o[k] = String(v); return JSON.stringify(o); } catch(e) { return '[FormData]'; }
            }
            if (b instanceof URLSearchParams) return b.toString().substring(0, 4000);
            if (b instanceof Blob || b instanceof ArrayBuffer) return `[binary ${b.size||b.byteLength||'?'} bytes]`;
            try { return JSON.stringify(b).substring(0, 4000); } catch(e) { return String(b).substring(0, 4000); }
        };
        const origFetch = window.fetch;
        window.fetch = async function(...args) {
            const url = typeof args[0] === 'string' ? args[0] : args[0]?.url || '?';
            const init = args[1] || (args[0] && typeof args[0] === 'object' ? args[0] : {});
            const method = init?.method || 'GET';
            const req_body = reqBodyToString(init?.body);
            const entry = { url, method, status: 0, req_body, body: '', ts: Date.now() };
            try {
                const resp = await origFetch.apply(this, args);
                entry.status = resp.status;
                try { entry.body = (await resp.clone().text()).substring(0, 4000); } catch(e) {}
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
            this.__sirin = { url, method, status: 0, req_body: '', body: '', ts: Date.now() };
            return origXhrOpen.apply(this, arguments);
        };
        XMLHttpRequest.prototype.send = function(body) {
            if (this.__sirin) this.__sirin.req_body = reqBodyToString(body);
            this.addEventListener('load', function() {
                if (this.__sirin) {
                    this.__sirin.status = this.status;
                    this.__sirin.body = this.responseText?.substring(0, 4000) || '';
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

/// Block until a captured request matching `url_substring` appears, or
/// `timeout_ms` elapses.  Returns the JSON-stringified entry on success,
/// or Err on timeout.
///
/// Use case: click a button → POST /api/checkout fires → assert against
/// its body.  Without `wait_for_request`, callers race between firing the
/// click and reading `captured_requests` before the request lands.
///
/// Auto-installs the network capture if not yet active.
pub fn wait_for_request(url_substring: &str, timeout_ms: u64) -> Result<String, String> {
    install_network_capture()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let needle = serde_json::to_string(url_substring)
        .map_err(|e| format!("escape pattern: {e}"))?;

    loop {
        // Look for a matching entry without holding any lock between polls.
        let js = format!(
            r#"(() => {{
                const arr = window.__sirin_net || [];
                const needle = {needle};
                const hit = arr.find(e => e.url && e.url.includes(needle));
                return hit ? JSON.stringify(hit) : '';
            }})()"#
        );
        let result = evaluate_js(&js)?;
        if !result.is_empty() && result != "null" {
            return Ok(result);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "wait_for_request: no captured request matched {url_substring:?} within {timeout_ms}ms"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
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
            node_id: Some(node_id),
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

/// Clear browser state for the current page's origin.
/// Wipes: cookies (all domains), localStorage, sessionStorage, IndexedDB,
/// caches.  Use between sequential tests to prevent cookie/auth leakage.
///
/// Does NOT close the browser or current tab — same Chrome process is
/// reused for speed.
pub fn clear_browser_state() -> Result<(), String> {
    use headless_chrome::protocol::cdp::Network;
    with_tab(|tab| {
        // Clear ALL cookies via CDP (covers all domains, not just current).
        tab.call_method(Network::ClearBrowserCookies(None))
            .map_err(|e| format!("clear cookies: {e}"))?;
        Ok(())
    })?;

    // Clear page-side storage via JS (localStorage, sessionStorage, IndexedDB).
    // Best-effort — some origins block storage access (sandboxed iframes).
    let _ = evaluate_js(r#"(async () => {
        try { localStorage.clear(); } catch(e) {}
        try { sessionStorage.clear(); } catch(e) {}
        try {
            if (window.indexedDB && indexedDB.databases) {
                const dbs = await indexedDB.databases();
                for (const db of dbs) {
                    if (db.name) indexedDB.deleteDatabase(db.name);
                }
            }
        } catch(e) {}
        try {
            if (window.caches) {
                const keys = await caches.keys();
                for (const k of keys) await caches.delete(k);
            }
        } catch(e) {}
        return 'cleared';
    })()"#);

    // Re-apply cached viewport — clear_state may have reloaded the page which
    // resets Emulation.setDeviceMetricsOverride (Issue #27).
    reapply_viewport();

    Ok(())
}

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
//  CONDITION-BASED WAITS  (P0 — Issue #19)
// ══════════════════════════════════════════════════════════════════════════════

/// Poll until the current URL **contains** `target` (substring match) or
/// matches the `/pattern/` regex.  Returns elapsed milliseconds on success.
///
/// ```json
/// {"action":"wait_for_url","target":"#/home","timeout":10000}
/// {"action":"wait_for_url","target":"/\\/wallet\\//","timeout":8000}
/// ```
pub fn wait_for_url(target: &str, timeout_ms: u64) -> Result<u64, String> {
    let is_regex = target.starts_with('/') && target.ends_with('/') && target.len() > 2;
    let re = if is_regex {
        let pattern = &target[1..target.len() - 1];
        Some(regex::Regex::new(pattern).map_err(|e| format!("wait_for_url: invalid regex: {e}"))?)
    } else {
        None
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let t0 = std::time::Instant::now();
    loop {
        let url = current_url().unwrap_or_default();
        let matched = if let Some(ref re) = re { re.is_match(&url) } else { url.contains(target) };
        if matched {
            return Ok(t0.elapsed().as_millis() as u64);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "wait_for_url: timeout after {timeout_ms}ms (pattern={target:?}, url={url:?})"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Poll until the network capture log has been stable (no new requests) for
/// at least `idle_ms` milliseconds.  Auto-installs the capture hook.
/// Returns elapsed milliseconds on success.
///
/// ```json
/// {"action":"wait_for_network_idle","timeout":15000}
/// ```
pub fn wait_for_network_idle(idle_ms: u64, timeout_ms: u64) -> Result<u64, String> {
    install_network_capture()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let t0 = std::time::Instant::now();
    let mut last_count = get_net_request_count()?;
    let mut stable_since = std::time::Instant::now();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let count = get_net_request_count()?;
        if count != last_count {
            last_count = count;
            stable_since = std::time::Instant::now();
        }
        if stable_since.elapsed().as_millis() as u64 >= idle_ms {
            return Ok(t0.elapsed().as_millis() as u64);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("wait_for_network_idle: timeout after {timeout_ms}ms"));
        }
    }
}

fn get_net_request_count() -> Result<usize, String> {
    let r = evaluate_js("(window.__sirin_net||[]).length")?;
    Ok(r.parse::<usize>().unwrap_or(0))
}

// ══════════════════════════════════════════════════════════════════════════════
//  NAMED SESSIONS  (P1 — Issue #19)
// ══════════════════════════════════════════════════════════════════════════════

/// Switch the active tab to a named session.  If the session doesn't exist
/// yet, a new tab is opened and registered under `session_id`.
///
/// All subsequent `with_tab` calls on the current thread will target this tab
/// until another `session_switch` call is made.
///
/// Typical flow:
/// ```json
/// {"action":"goto","target":"https://...","session_id":"buyer_a"}
/// {"action":"goto","target":"https://...","session_id":"buyer_b"}
/// {"action":"ax_find","role":"button","name":"下單","session_id":"buyer_a"}
/// ```
pub fn session_switch(session_id: &str) -> Result<usize, String> {
    ensure_open_reusing()?;
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_mut().ok_or("browser not open")?;

    if let Some(&idx) = inner.sessions.get(session_id) {
        if idx < inner.tabs.len() {
            inner.active = idx;
            return Ok(idx);
        }
        // Stale index — tab was closed, recreate below
        inner.sessions.remove(session_id);
    }

    // Create a new tab for this session
    let tab = inner.browser.new_tab().map_err(|e| format!("session_switch new_tab: {e}"))?;
    inner.tabs.push(tab);
    let idx = inner.tabs.len() - 1;
    inner.active = idx;
    inner.sessions.insert(session_id.to_string(), idx);
    tracing::debug!("[browser] created session '{session_id}' → tab {idx}");
    Ok(idx)
}

/// List all named sessions: returns Vec of (session_id, tab_index, url).
pub fn list_sessions() -> Result<Vec<(String, usize, String)>, String> {
    let guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_ref().ok_or("browser not open")?;
    let mut result: Vec<(String, usize, String)> = inner
        .sessions
        .iter()
        .map(|(id, &idx)| {
            let url = inner.tabs.get(idx).map(|t| t.get_url()).unwrap_or_default();
            (id.clone(), idx, url)
        })
        .collect();
    result.sort_by_key(|(_, idx, _)| *idx);
    Ok(result)
}

/// Close a named session and its associated tab.
pub fn close_session(session_id: &str) -> Result<(), String> {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_mut().ok_or("browser not open")?;
    let idx = inner.sessions.remove(session_id)
        .ok_or_else(|| format!("session '{session_id}' not found"))?;
    if inner.tabs.len() <= 1 {
        return Err("cannot close the last tab".into());
    }
    if idx < inner.tabs.len() {
        inner.tabs.remove(idx);
        if inner.active >= inner.tabs.len() {
            inner.active = inner.tabs.len() - 1;
        }
        // Shift down any session indices pointing past the removed tab
        for idx_ref in inner.sessions.values_mut() {
            if *idx_ref > idx { *idx_ref -= 1; }
        }
        // Mirror the reindex in the a11y enabled-tab tracker.
        crate::browser_ax::remove_a11y_tab(idx);
    }
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
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
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
    #[ignore] // needs Chrome; E2E test
    fn test_screenshot_jpeg_smoke() {
        navigate_and_screenshot("https://example.com").unwrap();
        let jpg = screenshot_jpeg(80).expect("jpeg");
        // JPEG magic: FF D8 FF
        assert_eq!(&jpg[..3], &[0xFF, 0xD8, 0xFF], "not a JPEG");
        assert!(jpg.len() > 100, "too small");
        assert!(jpg.len() < 500_000, "unexpectedly large (quality too high?)");
    }

    /// Regression test for #23 — `current_url()` and `page_title()` must
    /// reflect changes that happen *after* a navigation event the
    /// `headless_chrome` cache fails to observe (hash-only navigation,
    /// `document.title` mutated by JS, `about:blank` reset).
    ///
    /// Before the fix, `tab.get_url()` / `tab.get_title()` returned the cached
    /// snapshot from the last `Target.targetInfoChanged` event — which Chrome
    /// does not emit for any of these.  The fix routes both through
    /// `Runtime.evaluate` so we read live page state.
    #[test]
    #[ignore] // needs Chrome; integration test
    fn url_and_title_bypass_target_info_cache() {
        close();
        ensure_open(true).expect("launch");

        // 1. Baseline navigation — both APIs agree with the cache.
        navigate("data:text/html,<title>Initial</title><body>x</body>").expect("nav");
        assert_eq!(page_title().expect("t0"), "Initial");
        let url0 = current_url().expect("u0");
        assert!(url0.starts_with("data:text/html"), "url0={url0}");

        // 2. Mutate document.title in-page — Chrome fires NO target_info event,
        //    so `Tab::get_title()` would still report "Initial".
        evaluate_js(r#"document.title = "MutatedByJS""#).expect("set title");
        assert_eq!(
            page_title().expect("t1"),
            "MutatedByJS",
            "page_title() must read live document.title, not cached target_info"
        );

        // 3. Hash-only navigation — Chrome emits no Page.frameNavigated, so
        //    `Tab::get_url()` would still report the URL without the fragment.
        evaluate_js(r##"window.location.hash = "#/dashboard""##).expect("set hash");
        // Small settle — the JS assignment is sync but cache update is racey;
        // we want the test to prove our fix is robust without relying on luck.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let url1 = current_url().expect("u1");
        assert!(
            url1.contains("#/dashboard"),
            "current_url() must reflect hash change; got {url1}"
        );

        // 4. about:blank reset via JS — same cache-blindness as case 3.
        evaluate_js(r#"window.location.replace("about:blank")"#).expect("blank");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let url2 = current_url().expect("u2");
        assert_eq!(
            url2, "about:blank",
            "current_url() must reflect about:blank reset; got {url2}"
        );

        close();
        println!("✓ url_and_title_bypass_target_info_cache: all 3 cache-miss scenarios fixed");
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
