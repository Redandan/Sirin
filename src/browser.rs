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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Tracks the headless mode desired by the currently-running test.
/// Set by `set_test_headless_mode()` at test start so that mid-call
/// recovery in `with_tab()` can re-launch Chrome in the correct mode
/// (instead of falling back to `default_headless()` which is always `true`).
static TEST_DESIRED_HEADLESS: AtomicBool = AtomicBool::new(true);

/// Whether to inject a privacy CSS mask immediately before screenshot capture.
/// Defaults to `true` (fail-secure): password / credit-card / OTP fields are
/// blurred + colour-stripped so the captured PNG cannot leak plaintext into
/// `test_failures/`, vision LLM uploads, or GitHub bug reports.
///
/// Disable by:
/// - YAML test goal: `mask_sensitive: false` (test runner flips this for the
///   duration of that test, then restores the previous value).
/// - Env: `SIRIN_PRIVACY_MASK=0` (read once at process start).
///
/// See Issue #80 for the threat model.
static PRIVACY_MASK_ENABLED: AtomicBool = AtomicBool::new(true);

/// Signal that stops the CDP keepalive heartbeat thread.
/// Set to `true` by `close()` before dropping the browser session.
static HEARTBEAT_STOP: AtomicBool = AtomicBool::new(false);

// Per-thread tab index override.  Set by `session_switch()` so that each
// concurrent test thread always targets its own tab, bypassing the shared
// `inner.active` global pointer and eliminating the TOCTOU race:
//
//   Thread A: session_switch → inner.active = 2 → lock released
//   Thread B: session_switch → inner.active = 1 → lock released   ← clobbers A
//   Thread A: with_tab → reads inner.active = 1 → WRONG tab!
//
// With the thread-local, each thread reads its own index regardless of what
// other threads have written to `inner.active`.
thread_local! {
    static THREAD_ACTIVE_TAB: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Called by the test executor at test start to register the desired
/// headless mode.  Recovery paths read this to re-launch Chrome correctly
/// even if the process-level default is `headless=true`.
pub fn set_test_headless_mode(headless: bool) {
    TEST_DESIRED_HEADLESS.store(headless, Ordering::Relaxed);
}

/// Initialise the global privacy-mask toggle from `SIRIN_PRIVACY_MASK`.
/// Called once near process start.  `1`/`true`/`yes` → on (default),
/// `0`/`false`/`no` → off.  Any other value also leaves it on (fail-secure).
pub fn init_privacy_mask_from_env() {
    let on = !matches!(
        std::env::var("SIRIN_PRIVACY_MASK").ok().as_deref(),
        Some("0") | Some("false") | Some("FALSE") | Some("no") | Some("NO")
    );
    PRIVACY_MASK_ENABLED.store(on, Ordering::Relaxed);
}

/// Set the privacy-mask toggle for the currently-running test.  Returns the
/// previous value so the caller can restore it after the test finishes.
///
/// See [`PRIVACY_MASK_ENABLED`] for the rationale.
pub fn set_privacy_mask(enabled: bool) -> bool {
    PRIVACY_MASK_ENABLED.swap(enabled, Ordering::Relaxed)
}

/// Whether the privacy mask should be injected before the next screenshot.
pub fn privacy_mask_enabled() -> bool {
    PRIVACY_MASK_ENABLED.load(Ordering::Relaxed)
}

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
    // Stability flags — selected to reduce Chrome crashes on heavy JS/WebGL pages
    // (e.g. Flutter CanvasKit) without disabling GPU or breaking rendering.
    // --disable-dev-shm-usage:           prevent /dev/shm OOM on Linux; no-op on Windows
    // --disable-background-timer-throttling: JS timers run at full speed in background
    // --disable-backgrounding-occluded-windows: don't throttle off-screen tabs
    // --disable-renderer-backgrounding:  renderer priority stays high throughout test
    // --disable-hang-monitor:            don't kill renderer on slow Flutter bootstrap
    // NOT added: --disable-gpu / --no-sandbox — those break WebGL / Flutter CanvasKit
    //
    // Flutter rendering on Windows:
    //
    // --use-angle=swiftshader: software WebGL (no GPU driver crashes).
    //   Flutter detects software rendering → falls back to HTML renderer.
    //   HTML renderer uses real DOM: click/type/find all work normally.
    //
    // CDP keepalive note: headless_chrome drops the connection if no CDP
    //   events arrive for 30 s.  During Flutter JS init Chrome can be silent.
    //   Fix: call `install_capture` immediately after `goto` (before any wait)
    //   so Network/Page events from Flutter's resource loading flow keep the
    //   CDP connection alive.  See executor.rs pre-loop ordering.
    //
    // --disable-gpu was tried: forces full CPU compositing → Flutter startup
    //   screenshot takes 5-10 min (Skia software renderer for everything).
    //   REMOVED.
    //
    // --ignore-gpu-blocklist was tried: forces CanvasKit with SwiftShader →
    //   all-black screen (CanvasKit fails on SwiftShader).  REMOVED.
    // Default viewport — large enough that bottom UI (quick-login buttons,
    // test shortcuts, footers) is in-frame without a per-test set_viewport.
    // Flutter apps like redandan.github.io/#/login tuck 測試買家 / etc. below
    // the default 800×600 fold, causing vision LLMs to click the wrong
    // (visible) OAuth buttons.  Override via `SIRIN_DEFAULT_VIEWPORT=WxH` env.
    //
    // `--window-size` influences the initial Chrome window; the subsequent
    // `Emulation.setDeviceMetricsOverride` (done right after tab creation
    // below) is what actually determines what the page sees via innerWidth /
    // innerHeight and what CDP screenshots capture.
    let (default_w, default_h) = resolve_default_viewport();
    let window_size_arg = format!("--window-size={default_w},{default_h}");

    let stability_args: Vec<&str> = vec![
        "--disable-dev-shm-usage",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-renderer-backgrounding",
        "--disable-hang-monitor",
        // SwiftShader: software WebGL — prevents GPU driver crashes.
        // Flutter detects software rendering → HTML renderer (our intended mode).
        "--use-angle=swiftshader",
        &window_size_arg,
    ];
    let persistent_profile = resolve_persistent_profile_dir();
    let mut opts_builder = LaunchOptions::default_builder();
    opts_builder
        .headless(headless)
        .args(stability_args.iter().map(std::ffi::OsStr::new).collect());
    if let Some(dir) = persistent_profile.as_ref() {
        opts_builder.user_data_dir(Some(dir.clone()));
        tracing::info!(
            "[browser] using persistent profile at {} — login state will survive Chrome recovery",
            dir.display()
        );
    }
    // Extend idle_browser_timeout from the default 30 s to 1800 s (30 min).
    // headless_chrome has TWO loops that share this timeout:
    //   1. transport/mod.rs — the CDP WebSocket reader (marks connection closed on timeout)
    //   2. browser/mod.rs  — the browser event loop (processes TargetInfoChanged etc.)
    //
    // When the browser event loop times out and drops its receiver, TargetInfoChanged
    // arriving at the transport layer causes a SendError.  Our vendor patch (PR #118)
    // changes this from `break` to `continue`, preventing cascade death.  But the
    // browser event loop is still dead, meaning subsequent TargetInfoChanged events
    // show up as "WARN Couldn't send browser an event" in the log.
    //
    // 1800 s (30 min) covers worst-case LLM stall scenarios:
    //   - Gemini 429 retries (now 35 s with fast-fail): 35 × 3 = 105 s max
    //   - DeepSeek fallback also 429 (rare): 35 s more
    //   - Flutter init silence: up to 15 s
    //   Total worst-case: ~155 s — well within 1800 s margin.
    // Truly broken Chrome connections are detected by Sirin's mid-call recovery
    // code (src/browser.rs with_tab() retry loop) rather than relying on idle timeout.
    opts_builder.idle_browser_timeout(std::time::Duration::from_secs(1800));
    let opts = opts_builder
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

    // Apply the default viewport via CDP Emulation so the page's
    // window.innerWidth / innerHeight + CDP screenshots use our target
    // dimensions (not Chrome's default 800×600).  Done here, before any
    // `goto`, so Flutter's first layout sees the correct viewport.
    if let Err(e) = tab.call_method(Emulation::SetDeviceMetricsOverride {
        width: default_w,
        height: default_h,
        device_scale_factor: 1.0,
        mobile: false,
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
    }) {
        tracing::warn!("[browser] failed to apply default viewport {default_w}x{default_h}: {e}");
    }

    let tab_arc = tab.clone();
    *guard = Some(BrowserInner {
        browser,
        tabs: vec![tab],
        active: 0,
        headless,
        sessions: HashMap::new(),
        // Seed the cached viewport so re-apply-after-goto picks up the default.
        viewport: Some((default_w, default_h, 1.0, false)),
    });
    // Clear per-tab a11y state so the new session's tab index 0 starts fresh.
    crate::browser_ax::reset_a11y_enabled();
    // Spawn keepalive so the CDP transport loop stays alive on quiet pages.
    spawn_cdp_heartbeat(tab_arc);
    tracing::info!(
        "[browser] launched Chrome (headless={headless}, viewport={default_w}x{default_h})"
    );
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
    HEARTBEAT_STOP.store(true, Ordering::Relaxed);
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// Resolve the default Chrome viewport from `SIRIN_DEFAULT_VIEWPORT` env.
///
/// Format: `WxH` (e.g. `1920x1200`).  Falls back to **1440×1600** — large
/// enough to render the full login / dashboard of typical Flutter apps
/// (redandan.github.io's test-account shortcuts live around y=800-1100).
///
/// Dimensions are clamped to `[640..=3840]` × `[480..=4320]` so that a typo
/// can't launch Chrome with an absurd size.
fn resolve_default_viewport() -> (u32, u32) {
    const DEFAULT_W: u32 = 1440;
    const DEFAULT_H: u32 = 1600;
    const MIN_W: u32 = 640;
    const MAX_W: u32 = 3840;
    const MIN_H: u32 = 480;
    const MAX_H: u32 = 4320;

    let raw = match std::env::var("SIRIN_DEFAULT_VIEWPORT") {
        Ok(v) => v,
        Err(_) => return (DEFAULT_W, DEFAULT_H),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (DEFAULT_W, DEFAULT_H);
    }
    let parts: Vec<&str> = trimmed.split(['x', 'X', ',']).collect();
    if parts.len() != 2 {
        tracing::warn!(
            "[browser] SIRIN_DEFAULT_VIEWPORT='{raw}' not in WxH form; using {DEFAULT_W}x{DEFAULT_H}"
        );
        return (DEFAULT_W, DEFAULT_H);
    }
    let (w, h) = match (
        parts[0].trim().parse::<u32>(),
        parts[1].trim().parse::<u32>(),
    ) {
        (Ok(w), Ok(h)) => (w, h),
        _ => {
            tracing::warn!(
                "[browser] SIRIN_DEFAULT_VIEWPORT='{raw}' has non-numeric parts; using {DEFAULT_W}x{DEFAULT_H}"
            );
            return (DEFAULT_W, DEFAULT_H);
        }
    };
    let w = w.clamp(MIN_W, MAX_W);
    let h = h.clamp(MIN_H, MAX_H);
    (w, h)
}

/// Resolve an optional persistent Chrome `--user-data-dir` from the
/// `SIRIN_PERSISTENT_PROFILE` env var.
///
/// Accepted values:
///   - unset or empty → `None` (default — fresh profile per launch, legacy behaviour)
///   - `1` / `true` / `yes` → `<app_data_dir>/chrome-profile` (convenience default)
///   - anything else → treated as an absolute path to the profile directory
///
/// **Why opt-in:** persisting the profile means cookies / localStorage /
/// IndexedDB survive across Chrome relaunches — which is essential when
/// `with_tab` recovers from a transport-loop crash mid-test.  But it also
/// breaks strict test isolation for any test that doesn't explicitly
/// `clear_state` in its fixture.  Default stays off so existing tests
/// behave identically; enable per-session via the env var when a flow
/// needs login state to survive a Flutter hash-route race.
fn resolve_persistent_profile_dir() -> Option<std::path::PathBuf> {
    let raw = std::env::var("SIRIN_PERSISTENT_PROFILE").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "0" | "false" | "no" | "off" => None,
        "1" | "true" | "yes" | "on" => {
            let dir = crate::platform::app_data_dir().join("chrome-profile");
            let _ = std::fs::create_dir_all(&dir);
            Some(dir)
        }
        _ => {
            let dir = std::path::PathBuf::from(trimmed);
            let _ = std::fs::create_dir_all(&dir);
            Some(dir)
        }
    }
}

/// Interval between CDP keepalive pulses.  headless_chrome's transport loop
/// exits after ~30 s of silence; firing every 10 s gives us three chances
/// per window, so a single dropped pulse (transient jitter) won't kill the
/// connection.
///
/// 25 s was tried and left only one pulse per window — too brittle when an
/// LLM turn (5-15 s) lands next to a long action.
const HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// Spawns a background thread that calls `Target.getTargetInfo` every
/// `HEARTBEAT_INTERVAL_SECS` seconds, generating CDP round-trips that
/// prevent headless_chrome's transport loop from timing out on quiet pages
/// (e.g. idle Flutter app waiting for user interaction).
///
/// **What this fixes:** 30 s of CDP silence on Flutter/Canvas pages —
/// common when the ReAct loop is waiting for the LLM and no network /
/// Page events fire in between.
///
/// **What this does NOT fix:** mid-call races where Chrome fires
/// `Target.targetInfoChanged` after a hash-route navigation and
/// headless_chrome's internal mpsc channel to a subscriber is already
/// closed (`SendError`).  That unwinds the transport in milliseconds —
/// no heartbeat interval can catch it.  See `with_tab` one-shot recovery
/// + `SIRIN_PERSISTENT_PROFILE` for session-survival across such races.
///
/// The heartbeat uses `get_target_info` (returns tab metadata, ~50 bytes)
/// rather than `Runtime.evaluate` to avoid JS execution overhead on pages
/// that may be mid-transition.  Exits on the first failure — recovery
/// re-spawns a fresh heartbeat when `ensure_open` relaunches Chrome.
fn spawn_cdp_heartbeat(tab: Arc<Tab>) {
    HEARTBEAT_STOP.store(false, Ordering::Relaxed);
    std::thread::spawn(move || {
        loop {
            for _ in 0..HEARTBEAT_INTERVAL_SECS {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if HEARTBEAT_STOP.load(Ordering::Relaxed) {
                    tracing::debug!("[browser] heartbeat stopped");
                    return;
                }
            }
            match tab.get_target_info() {
                Ok(_) => tracing::debug!("[browser] heartbeat ok"),
                Err(e) => {
                    tracing::debug!("[browser] heartbeat exiting on error: {e}");
                    return; // Chrome/tab is dead — recovery will spawn a new one
                }
            }
        }
    });
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
///
/// Fixes: #35 — when a test requests `browser_headless=false` and Chrome crashes
/// mid-test, `ensure_open_reusing()` used to call `ensure_open(default_headless())`
/// which always returns `true`, relaunching Chrome in headless mode and causing
/// Flutter/WebGL content to render all-black for the rest of the test.
/// Now uses `TEST_DESIRED_HEADLESS` so crash recovery respects the test's mode.
pub fn ensure_open_reusing() -> Result<bool, String> {
    if is_open() {
        return Ok(false);
    }
    // Use the mode last registered by a test (set_test_headless_mode), or the
    // process default if no test has run yet.  This ensures crash recovery
    // during Flutter/WebGL tests re-launches Chrome in the correct visible mode.
    ensure_open(TEST_DESIRED_HEADLESS.load(std::sync::atomic::Ordering::Relaxed))
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

/// Navigate browser history back one step (equivalent to pressing the browser back button).
///
/// Flutter SPA pushed-route transitions are **hash-route changes**, not full page navigations,
/// so CDP `Page.navigateToHistoryEntry` (which headless_chrome doesn't expose) is not needed.
/// `history.back()` is sufficient and more compatible.
///
/// After calling JS `history.back()`, this waits for the Flutter AX tree to settle
/// (≥10 nodes within 8 s) so the next `shadow_click` / `shadow_dump` sees a live page —
/// the same strategy used by `shadow_click` itself via `wait_for_ax_ready`.
///
/// `wait_ms`: optional extra settle delay (ms) appended **after** AX-ready, useful when
/// Flutter plays a pop-route exit animation that finishes after semantics rebuild.
pub fn go_back(wait_ms: u64) -> Result<(), String> {
    // JS history.back() for SPA hash-route back navigation.
    evaluate_js("history.back()")?;

    // Flutter doesn't fire Page.frameNavigated for hash-route transitions.
    // Poll the AX tree until it recovers (≥10 nodes = page has rebuilt semantics).
    // Ignore the error — if AX tree never grows it's a test issue, not a browser crash.
    let _ = crate::browser_ax::wait_for_ax_ready(10, 8000);

    // Re-apply cached viewport override (same as navigate() does on full nav).
    reapply_viewport();

    if wait_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
    }
    Ok(())
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

// ── Privacy mask (Issue #80) ─────────────────────────────────────────────────
//
// Goal: prevent password / credit-card / OTP plaintext from leaking into
// failure screenshots stored in `test_failures/`, uploaded to vision LLMs,
// or pasted into GitHub bug reports.  We inject CSS that blurs and
// colour-strips matching inputs immediately before capture, then remove it
// so subsequent test steps see the page unchanged.
//
// JS runs synchronously inside the page (Runtime.evaluate) so the style is
// applied before the next CDP frame.  Both inject and remove are best-effort
// — failures are logged but never abort the screenshot.

const PRIVACY_MASK_STYLE_ID: &str = "__sirin_privacy_mask__";

/// CSS rule list that defines which fields the mask covers.  Centralised so
/// tests can assert against it.
const PRIVACY_MASK_CSS: &str = r#"
input[type="password"],
input[autocomplete*="cc-"],
input[autocomplete*="one-time-code"],
input[autocomplete*="otp"],
input[name*="password" i],
input[name*="passwd" i],
input[name*="ssn" i],
input[name*="credit" i],
input[name*="card-number" i],
input[name*="cardnumber" i],
input[name*="cvv" i],
input[name*="cvc" i],
input[aria-label*="password" i],
input[aria-label*="\5BC6\78BC"],
input[aria-label*="credit" i],
input[aria-label*="ssn" i],
[data-sensitive],
[data-sirin-mask="true"] {
  filter: blur(8px) !important;
  background: #1A1A1A !important;
  color: transparent !important;
  caret-color: transparent !important;
  text-shadow: none !important;
  -webkit-text-security: disc !important;
}
"#;

fn build_inject_js() -> String {
    // Single-shot inject: idempotent — if a previous screenshot left the style
    // behind for any reason, replace it.  Returns 1 on success.
    let css = PRIVACY_MASK_CSS.replace('`', r"\`");
    format!(
        r#"(function() {{
  try {{
    var id = "{id}";
    var prev = document.getElementById(id);
    if (prev) prev.remove();
    var s = document.createElement("style");
    s.id = id;
    s.textContent = `{css}`;
    (document.head || document.documentElement).appendChild(s);
    return 1;
  }} catch (e) {{ return 0; }}
}})()"#,
        id = PRIVACY_MASK_STYLE_ID,
        css = css,
    )
}

fn build_remove_js() -> String {
    format!(
        r#"(function() {{
  try {{
    var el = document.getElementById("{id}");
    if (el) el.remove();
    return 1;
  }} catch (e) {{ return 0; }}
}})()"#,
        id = PRIVACY_MASK_STYLE_ID,
    )
}

/// Inject the privacy-mask `<style>` element if the global toggle is on.
/// Errors are logged at debug level and never propagate — masking is a
/// defence-in-depth, not a hard precondition.
fn inject_privacy_mask(tab: &Tab) {
    if !privacy_mask_enabled() {
        return;
    }
    if let Err(e) = tab.evaluate(&build_inject_js(), false) {
        tracing::debug!("[privacy_mask] inject failed (non-fatal): {e}");
    }
}

/// Remove the privacy-mask `<style>` element so subsequent test steps see
/// the page unchanged.  Best-effort.
fn remove_privacy_mask(tab: &Tab) {
    if !privacy_mask_enabled() {
        return;
    }
    if let Err(e) = tab.evaluate(&build_remove_js(), false) {
        tracing::debug!("[privacy_mask] remove failed (non-fatal): {e}");
    }
}

// ── Action indicator + HIDE_FOR_TOOL_USE (Issue #75) ────────────────────────
//
// When `ACTION_INDICATOR_ENABLED` is on, the test executor injects a small
// in-page DOM overlay (right-bottom badge + faint border) so the user can SEE
// that Sirin is driving the page.  The badge text updates as the LLM picks
// each next action.
//
// Two orthogonal hide paths run before every screenshot / AX-tree read:
//
//   1. The indicator itself — Sirin's own UI must never appear in failure
//      screenshots or pollute the AX-tree observation we feed back to the LLM.
//   2. `HIDE_FOR_TOOL_USE`: any element a *test author* tags with
//      `data-sirin-hide` (or the CiC-compatible alias `data-claude-hide`) gets
//      `visibility:hidden` while the agent observes.  Useful for blanking out
//      a banner or ad while the agent is asserting page correctness, and as a
//      lightweight prompt-injection mitigation for content the page itself
//      wants the agent to ignore.
//
// IMPORTANT — this is **UX, not a security boundary**.  The agent can still
// read the source DOM via `eval` to bypass it.  The contract is "don't show
// this to the LLM in its default observation channels (AX tree / screenshot)".
//
// Ordering vs privacy mask (Issue #80): both inject distinct `<style>` nodes
// keyed by different IDs and both remove themselves after capture, so they
// compose cleanly — no inject/remove interlock required.

const ACTION_INDICATOR_BORDER_ID: &str = "__sirin_indicator_border__";
const ACTION_INDICATOR_BADGE_ID:  &str = "__sirin_indicator_badge__";
const ACTION_INDICATOR_STYLE_ID:  &str = "__sirin_indicator_style__";
const HIDE_FOR_TOOL_USE_STYLE_ID: &str = "__sirin_hide_for_tool_use__";

/// CSS that hides any element annotated with `data-sirin-hide` or
/// `data-claude-hide` while a tool is observing the page.  Also hides
/// the action indicator itself so it never lands in screenshots.
const HIDE_FOR_TOOL_USE_CSS: &str = r#"
[data-sirin-hide], [data-claude-hide],
#__sirin_indicator_border__,
#__sirin_indicator_badge__ {
  visibility: hidden !important;
}
"#;

/// Process-wide enable for the action indicator.  Off by default so headless
/// CI runs and any caller that hasn't opted in keeps a clean DOM.  Flipped on
/// by the test executor for runs with `show_action_indicator: true`.
static ACTION_INDICATOR_ENABLED: AtomicBool = AtomicBool::new(false);

/// Toggle the in-page action indicator.  Returns the previous value so the
/// caller can restore it after a test finishes (mirrors `set_privacy_mask`).
pub fn set_action_indicator(enabled: bool) -> bool {
    ACTION_INDICATOR_ENABLED.swap(enabled, Ordering::Relaxed)
}

/// Whether the action indicator is currently enabled.
pub fn action_indicator_enabled() -> bool {
    ACTION_INDICATOR_ENABLED.load(Ordering::Relaxed)
}

/// Build the JS that injects (or refreshes) the in-page indicator.  Called
/// at test start and re-called on each action to update the badge text.
fn build_indicator_inject_js(action: &str) -> String {
    // Escape backticks + backslashes so the action text can never break out
    // of the template literal.  Truncate to keep the badge readable.
    let truncated: String = action.chars().take(60).collect();
    let safe = truncated.replace('\\', r"\\").replace('`', r"\`");
    format!(
        r#"(function() {{
  try {{
    var BORDER_ID = "{border}";
    var BADGE_ID  = "{badge}";
    var STYLE_ID  = "{style}";
    var label = `Sirin: ${{`{safe}`}}`;
    if (!document.getElementById(STYLE_ID)) {{
      var s = document.createElement("style");
      s.id = STYLE_ID;
      s.textContent = "@keyframes __sirin_pulse{{0%,100%{{box-shadow:0 0 12px #00FFA3,inset 0 0 12px rgba(0,255,163,.1)}}50%{{box-shadow:0 0 24px #00FFA3,inset 0 0 20px rgba(0,255,163,.2)}}}}";
      (document.head || document.documentElement).appendChild(s);
    }}
    var border = document.getElementById(BORDER_ID);
    if (!border) {{
      border = document.createElement("div");
      border.id = BORDER_ID;
      border.setAttribute("aria-hidden", "true");
      border.style.cssText = "position:fixed;inset:0;pointer-events:none;border:2px solid #00FFA3;border-radius:2px;z-index:2147483646;box-shadow:0 0 12px #00FFA3,inset 0 0 12px rgba(0,255,163,.1);animation:__sirin_pulse 2s ease-in-out infinite;";
      document.body.appendChild(border);
    }}
    var badge = document.getElementById(BADGE_ID);
    if (!badge) {{
      badge = document.createElement("div");
      badge.id = BADGE_ID;
      badge.setAttribute("aria-hidden", "true");
      badge.style.cssText = "position:fixed;bottom:16px;right:16px;z-index:2147483647;padding:6px 12px;background:#1A1A1A;color:#00FFA3;border:1px solid #333;border-radius:4px;font-family:monospace;font-size:12px;pointer-events:none;max-width:60vw;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;";
      document.body.appendChild(badge);
    }}
    badge.textContent = label;
    return 1;
  }} catch (e) {{ return 0; }}
}})()"#,
        border = ACTION_INDICATOR_BORDER_ID,
        badge  = ACTION_INDICATOR_BADGE_ID,
        style  = ACTION_INDICATOR_STYLE_ID,
        safe   = safe,
    )
}

/// Build the JS that removes the indicator + style.  Idempotent.
fn build_indicator_remove_js() -> String {
    format!(
        r#"(function() {{
  try {{
    ["{border}", "{badge}", "{style}"].forEach(function(id) {{
      var el = document.getElementById(id);
      if (el) el.remove();
    }});
    return 1;
  }} catch (e) {{ return 0; }}
}})()"#,
        border = ACTION_INDICATOR_BORDER_ID,
        badge  = ACTION_INDICATOR_BADGE_ID,
        style  = ACTION_INDICATOR_STYLE_ID,
    )
}

/// Inject (or refresh) the action indicator with the given action label.
/// No-op if [`action_indicator_enabled`] is `false`.  Failures are demoted
/// to debug log — never propagate.
pub fn show_action_indicator(action: &str) {
    if !action_indicator_enabled() {
        return;
    }
    let js = build_indicator_inject_js(action);
    let _ = with_tab(|tab| {
        if let Err(e) = tab.evaluate(&js, false) {
            tracing::debug!("[indicator] inject failed (non-fatal): {e}");
        }
        Ok(())
    });
}

/// Remove the action indicator immediately.  Use at end-of-test cleanup.
/// Best-effort.
pub fn hide_action_indicator() {
    let js = build_indicator_remove_js();
    let _ = with_tab(|tab| {
        if let Err(e) = tab.evaluate(&js, false) {
            tracing::debug!("[indicator] remove failed (non-fatal): {e}");
        }
        Ok(())
    });
}

fn build_hide_for_tool_use_inject_js() -> String {
    let css = HIDE_FOR_TOOL_USE_CSS.replace('`', r"\`");
    format!(
        r#"(function() {{
  try {{
    var id = "{id}";
    var prev = document.getElementById(id);
    if (prev) prev.remove();
    var s = document.createElement("style");
    s.id = id;
    s.textContent = `{css}`;
    (document.head || document.documentElement).appendChild(s);
    return 1;
  }} catch (e) {{ return 0; }}
}})()"#,
        id  = HIDE_FOR_TOOL_USE_STYLE_ID,
        css = css,
    )
}

fn build_hide_for_tool_use_remove_js() -> String {
    format!(
        r#"(function() {{
  try {{
    var el = document.getElementById("{id}");
    if (el) el.remove();
    return 1;
  }} catch (e) {{ return 0; }}
}})()"#,
        id = HIDE_FOR_TOOL_USE_STYLE_ID,
    )
}

/// Inject HIDE_FOR_TOOL_USE CSS — hides Sirin's own indicator AND any
/// element tagged `data-sirin-hide` / `data-claude-hide`.  Always runs
/// before screenshot; cheap (single style insert).
fn inject_hide_for_tool_use(tab: &Tab) {
    if let Err(e) = tab.evaluate(&build_hide_for_tool_use_inject_js(), false) {
        tracing::debug!("[hide_for_tool_use] inject failed (non-fatal): {e}");
    }
}

fn remove_hide_for_tool_use(tab: &Tab) {
    if let Err(e) = tab.evaluate(&build_hide_for_tool_use_remove_js(), false) {
        tracing::debug!("[hide_for_tool_use] remove failed (non-fatal): {e}");
    }
}

/// All-black PNG threshold: real Flutter/HTML pages render to ≥ 15 KB.
/// A < 14 KB PNG almost always means Chrome recovered but Flutter hasn't
/// painted its first frame yet.
const SCREENSHOT_BLACK_THRESHOLD_BYTES: usize = 14_000;

pub fn screenshot() -> Result<Vec<u8>, String> {
    // Auto-retry up to 2 extra times if the screenshot is all-black.
    // Flutter WebGL / CanvasKit needs a moment to re-render after Chrome
    // recovery; waiting 2 s and retrying is far cheaper than failing the test.
    for attempt in 0u8..3 {
        if attempt > 0 {
            tracing::warn!(
                "[screenshot] result is all-black (attempt {}/3) — waiting 2s for Flutter re-render",
                attempt
            );
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        let bytes = with_tab(|tab| {
            inject_privacy_mask(tab);
            inject_hide_for_tool_use(tab);
            let res = tab
                .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
                .map_err(|e| format!("screenshot: {e}"));
            remove_hide_for_tool_use(tab);
            remove_privacy_mask(tab);
            res
        })?;
        if bytes.len() >= SCREENSHOT_BLACK_THRESHOLD_BYTES || attempt == 2 {
            return Ok(bytes);
        }
    }
    unreachable!()
}

/// Returns the current tab's URL, or an empty string if the browser is not open.
pub fn get_current_url() -> Result<String, String> {
    with_tab(|tab| Ok(tab.get_url()))
}

/// Capture current tab as JPEG with the given quality (1-100).
/// Prefer over `screenshot()` when streaming (e.g. Monitor view) — JPEG 80
/// compresses Flutter / typical UI screens to ~50 KB vs 500 KB for PNG.
pub fn screenshot_jpeg(quality: u8) -> Result<Vec<u8>, String> {
    with_tab(|tab| {
        let q = quality.clamp(1, 100) as u32;
        inject_privacy_mask(tab);
        inject_hide_for_tool_use(tab);
        let res = tab.capture_screenshot(
            CaptureScreenshotFormatOption::Jpeg,
            Some(q),
            None,
            true,
        )
        .map_err(|e| format!("screenshot_jpeg: {e}"));
        remove_hide_for_tool_use(tab);
        remove_privacy_mask(tab);
        res
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

/// Returns true when `s` is a CSS selector (starts with `#`, `.`, `[`, `:`, `*`,
/// or is an ASCII-only string that looks like a tag name / combinator chain).
/// Plain-text labels (e.g. Chinese menu items) return false → JS text-click.
fn looks_like_css_selector(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() { return false; }
    // Explicit CSS sigils
    if matches!(s.chars().next(), Some('#') | Some('.') | Some('[') | Some(':') | Some('*')) {
        return true;
    }
    // If it contains non-ASCII characters it is almost certainly text content, not CSS
    if !s.is_ascii() {
        return false;
    }
    // All-ASCII: treat as CSS (tag name, class combo, attribute, etc.)
    true
}

pub fn click(selector: &str) -> Result<(), String> {
    if looks_like_css_selector(selector) {
        // Standard CSS selector path
        return with_tab(|tab| {
            tab.wait_for_element(selector)
                .map_err(|e| format!("click – find '{selector}': {e}"))?
                .click()
                .map(|_| ())
                .map_err(|e| format!("click '{selector}': {e}"))
        });
    }
    // Plain-text label (e.g. "使用用戶名密碼登入"): JavaScript innerText search.
    //
    // WHY NOT XPath: `wait_for_xpath` blocks for up to 30 seconds when the element
    // is absent. During that silence the headless_chrome CDP WebSocket times out,
    // triggering a false "Chrome crashed" recovery. JavaScript evaluate() is a
    // one-shot CDP call — it never causes CDP silence regardless of the result.
    //
    // Strategy: iterate all DOM elements in document order, keep the LAST match.
    // In Flutter HTML renderer, parent containers appear before their leaf children,
    // so the last element with matching innerText is the innermost (most-clickable) one.
    //
    // Retry up to 10 times × 1 s so dynamic content has time to appear.
    let safe = selector
        .replace('\\', "\\\\")
        .replace('`', "\\`");
    let js = format!(
        r#"(function(){{
            var target=`{safe}`;
            var all=document.querySelectorAll('*');
            var match=null;
            for(var i=0;i<all.length;i++){{
                var t=(all[i].innerText||'').trim();
                if(t===target)match=all[i];
            }}
            if(match){{match.click();return'clicked:'+match.tagName+(match.id?'#'+match.id:'');}}
            return'not-found';
        }})()"#
    );
    for attempt in 0..10u32 {
        let result = evaluate_js(&js)?;
        if !result.contains("not-found") {
            return Ok(());
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
    }
    Err(format!("click – text-find '{selector}': element not found after 10s"))
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

// ── DOM snapshot + ref_id (Issue #74) ────────────────────────────────────────
//
// CiC-style stable element references: snapshot the page into a flat list of
// interactable elements, give each a short ref_id (e1, e2, …), stamp the id
// onto the element as `data-sirin-ref="eN"` so subsequent actions can locate
// it via a selector that survives re-renders better than a brittle CSS path.
//
// The map is also kept on `window.__sirinRefMap` (WeakRef) so the LLM can
// re-snapshot mid-test and stale refs auto-evict instead of pointing at the
// wrong element.  ref_ids RESET on navigation (Chrome wipes window state),
// which is intentional — the caller should re-snapshot after `goto`.

/// Default cap on snapshot element count.  Pages with >200 interactables
/// burn LLM context for marginal value; truncate + flag `truncated:true`.
pub const DOM_SNAPSHOT_DEFAULT_MAX: usize = 200;

/// JS payload that walks the visible DOM, assigns ref_ids, stamps
/// `data-sirin-ref` and returns a JSON string `{url,elements,truncated}`.
fn dom_snapshot_js(max: usize) -> String {
    format!(
        r#"(function(){{
  var MAX = {max};
  window.__sirinRefMap = window.__sirinRefMap || {{}};
  window.__sirinRefCounter = window.__sirinRefCounter || 0;
  function getRole(el){{
    var role = el.getAttribute && el.getAttribute('role');
    if(role) return role;
    var tag = el.tagName.toLowerCase();
    var type = (el.getAttribute && el.getAttribute('type')) || '';
    var map = {{a:'link', button:'button', select:'combobox',
                textarea:'textbox', h1:'heading', h2:'heading',
                h3:'heading', h4:'heading', h5:'heading', h6:'heading'}};
    if(tag==='input'){{
      if(type==='checkbox') return 'checkbox';
      if(type==='radio') return 'radio';
      if(type==='submit'||type==='button') return 'button';
      return 'textbox';
    }}
    return map[tag] || 'generic';
  }}
  function getName(el){{
    var n = (el.getAttribute && (el.getAttribute('aria-label')
                              || el.getAttribute('placeholder')
                              || el.getAttribute('title')
                              || el.getAttribute('alt'))) || '';
    if(!n && el.children && el.children.length===0){{
      n = (el.textContent || '').trim();
    }}
    return (n || '').trim().slice(0, 100);
  }}
  function isVisible(el){{
    if(!el || !el.getBoundingClientRect) return false;
    var s = window.getComputedStyle(el);
    if(s.display==='none' || s.visibility==='hidden') return false;
    return el.offsetWidth > 0 && el.offsetHeight > 0;
  }}
  function isInteresting(el){{
    var tag = el.tagName.toLowerCase();
    if(['script','style','meta','link','noscript','head'].indexOf(tag) >= 0) return false;
    if(['a','button','input','select','textarea'].indexOf(tag) >= 0) return true;
    if(el.getAttribute){{
      if(el.getAttribute('onclick') !== null) return true;
      if(el.getAttribute('tabindex') !== null) return true;
      if(el.getAttribute('contenteditable') === 'true'
        || el.getAttribute('contenteditable') === '') return true;
      var role = el.getAttribute('role');
      if(role && ['button','link','tab','checkbox','radio','combobox','menuitem','option']
                  .indexOf(role) >= 0) return true;
    }}
    return false;
  }}
  function getOrCreateRef(el){{
    var existing = el.getAttribute && el.getAttribute('data-sirin-ref');
    if(existing){{
      var ref = window.__sirinRefMap[existing];
      if(ref && ref.deref && ref.deref() === el) return existing;
    }}
    var id = 'e' + (++window.__sirinRefCounter);
    try {{ window.__sirinRefMap[id] = new WeakRef(el); }}
    catch(e) {{ window.__sirinRefMap[id] = {{deref:function(){{return el;}}}}; }}
    if(el.setAttribute) el.setAttribute('data-sirin-ref', id);
    return id;
  }}
  // GC sweep: drop refs whose target is gone.
  for(var k in window.__sirinRefMap){{
    var w = window.__sirinRefMap[k];
    if(w && w.deref && !w.deref()) delete window.__sirinRefMap[k];
  }}
  var out = [];
  var truncated = false;
  var all = document.querySelectorAll('*');
  for(var i=0;i<all.length;i++){{
    var el = all[i];
    if(!isInteresting(el)) continue;
    if(!isVisible(el)) continue;
    if(out.length >= MAX){{ truncated = true; break; }}
    var rect = el.getBoundingClientRect();
    var ref = getOrCreateRef(el);
    out.push({{
      ref: ref,
      role: getRole(el),
      name: getName(el),
      tag: el.tagName.toLowerCase(),
      bbox: [Math.round(rect.x), Math.round(rect.y),
             Math.round(rect.width), Math.round(rect.height)],
      href: (el.getAttribute && el.getAttribute('href')) || null,
      type: (el.getAttribute && el.getAttribute('type')) || null
    }});
  }}
  return JSON.stringify({{
    url: window.location.href,
    count: out.length,
    truncated: truncated,
    elements: out
  }});
}})()"#
    )
}

/// Walk the page DOM and return a JSON snapshot of interactable elements.
/// Caps at `max` (default `DOM_SNAPSHOT_DEFAULT_MAX`); when exceeded the
/// returned object includes `truncated: true`.
pub fn dom_snapshot(max: usize) -> Result<serde_json::Value, String> {
    let cap = if max == 0 { DOM_SNAPSHOT_DEFAULT_MAX } else { max };
    let raw = evaluate_js(&dom_snapshot_js(cap))?;
    serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| {
        let preview: String = raw.chars().take(200).collect();
        format!("dom_snapshot: parse JS result: {e} (raw: {preview})")
    })
}

/// Resolve a ref_id (from `dom_snapshot`) to a CSS selector usable by
/// `click` / `type` / `get_text` / `element_exists` / `hover`.
/// Errors if the ref is unknown or the element has been removed.
pub fn resolve_ref(ref_id: &str) -> Result<String, String> {
    if !is_valid_ref_id(ref_id) {
        return Err(format!("invalid ref_id format: {ref_id:?} (expected eN)"));
    }
    let probe = format!(
        r#"(function(){{
            var m = window.__sirinRefMap || {{}};
            var w = m['{ref_id}'];
            var el = w && w.deref && w.deref();
            if(!el) return 'GONE';
            if(!document.body.contains(el)) return 'DETACHED';
            if(el.getAttribute('data-sirin-ref') !== '{ref_id}'){{
              el.setAttribute('data-sirin-ref', '{ref_id}');
            }}
            return 'OK';
        }})()"#
    );
    let status = evaluate_js(&probe)?;
    let trimmed = status.trim().trim_matches('"');
    if trimmed != "OK" {
        return Err(format!(
            "ref_id '{ref_id}' {trimmed} — page may have re-rendered or navigated; \
             call dom_snapshot again"
        ));
    }
    Ok(ref_selector(ref_id))
}

/// Pure helper: validate a ref_id against the `eN` schema (e1, e42, …).
pub fn is_valid_ref_id(ref_id: &str) -> bool {
    let bytes = ref_id.as_bytes();
    if bytes.len() < 2 { return false; }
    if bytes[0] != b'e' { return false; }
    bytes[1..].iter().all(|b| b.is_ascii_digit())
}

/// Pure helper: build the CSS selector that matches an element stamped by
/// `dom_snapshot`.
pub fn ref_selector(ref_id: &str) -> String {
    format!(r#"[data-sirin-ref="{ref_id}"]"#)
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

/// Page-coordinate context — relates CSS pixels (the unit CDP expects for
/// `Input.dispatchMouseEvent`) to the physical pixel size of `screenshot()`'s
/// PNG output.  On HiDPI / Retina monitors `device_pixel_ratio > 1.0` and the
/// two coord systems differ by that factor; on a plain 1080p panel they are
/// equal.  See Issue #79.
#[derive(Debug, Clone, Copy)]
pub struct ViewportContext {
    /// `window.innerWidth` — CSS pixels.
    pub viewport_width: f64,
    /// `window.innerHeight` — CSS pixels.
    pub viewport_height: f64,
    /// `window.devicePixelRatio` — physical_px / css_px.
    pub device_pixel_ratio: f64,
}

impl ViewportContext {
    /// Convert a screenshot-pixel (physical) coordinate into a CSS-pixel
    /// coordinate suitable for `Input.dispatchMouseEvent`.
    pub fn screenshot_to_css(&self, pixel_x: f64, pixel_y: f64) -> (f64, f64) {
        let dpr = if self.device_pixel_ratio > 0.0 {
            self.device_pixel_ratio
        } else {
            1.0
        };
        (pixel_x / dpr, pixel_y / dpr)
    }
}

/// Probe the current page for viewport metrics needed to translate
/// screenshot coordinates → CSS coordinates.  Cheap (single JS eval).
pub fn viewport_context() -> Result<ViewportContext, String> {
    let raw = evaluate_js(
        r#"JSON.stringify({
            vw:  window.innerWidth,
            vh:  window.innerHeight,
            dpr: window.devicePixelRatio || 1
        })"#,
    )?;
    #[derive(serde::Deserialize)]
    struct Probe {
        vw: f64,
        vh: f64,
        dpr: f64,
    }
    let p: Probe = serde_json::from_str(&raw)
        .map_err(|e| format!("viewport_context parse '{raw}': {e}"))?;
    Ok(ViewportContext {
        viewport_width: p.vw,
        viewport_height: p.vh,
        device_pixel_ratio: p.dpr,
    })
}

/// Click at exact viewport coordinates (x, y) **in CSS pixels**.  Useful for
/// Canvas-based UIs (e.g. Flutter Web) where CSS selectors don't work.
///
/// If your `(x, y)` came from a screenshot pixel position (e.g. a vision LLM
/// reading the PNG returned by `screenshot()`), use `click_point_screenshot`
/// instead — on HiDPI monitors the two coord systems differ by
/// `devicePixelRatio` and this entry point will click the wrong place.
pub fn click_point(x: f64, y: f64) -> Result<(), String> {
    with_tab(|tab| {
        // Point imported at module level
        tab.click_point(Point { x, y })
            .map_err(|e| format!("click_point({x},{y}): {e}"))?;
        Ok(())
    })
}

/// Click at a coordinate read from the screenshot PNG (physical pixels).
/// Auto-converts to CSS pixels using `window.devicePixelRatio` so the click
/// hits the same element the vision LLM saw in the image.  See Issue #79.
pub fn click_point_screenshot(px: f64, py: f64) -> Result<(), String> {
    let ctx = viewport_context().unwrap_or(ViewportContext {
        viewport_width: 0.0,
        viewport_height: 0.0,
        device_pixel_ratio: 1.0,
    });
    let (cx, cy) = ctx.screenshot_to_css(px, py);
    click_point(cx, cy)
}

// ── Flutter Shadow DOM helpers ────────────────────────────────────────────────
// Flutter Web (CanvasKit) creates an accessibility overlay inside
// `flt-glass-pane`'s **open** shadow root.  We query it directly via JS,
// bypassing the CDP AX protocol entirely (no strict-enum crash, no stale
// backendNodeId).  `enable_a11y` must still be called first to trigger Flutter
// to build the semantics overlay.

/// Find an element inside Flutter's `flt-semantics-host` shadow DOM.
/// Returns `(center_x, center_y, label)` or an error.
///
/// Retries up to 5× (600 ms apart) if `flt-semantics-host` is still empty —
/// Flutter populates it asynchronously after `enable_a11y` / placeholder click.
pub fn shadow_find(role: Option<&str>, name_regex: Option<&str>) -> Result<(f64, f64, String), String> {
    // Recovery ladder for the "flt-semantics-host is empty" case (i.e. Flutter
    // hasn't built its a11y bridge yet — common right after a route change):
    //
    //   attempt 0: try immediately (cold call)
    //   attempt 1: if first call hit "is empty", actively trigger
    //              `enable_flutter_semantics()` + 800ms wait, then retry
    //   attempt 2-4: short 400ms poll between retries (bootstrap already done)
    //
    // Worst-case wait (host stays empty): 800 + 400×3 = 2.0 s.
    // Common case (Flutter just needs a nudge): 800 ms.
    // Pre-119efdc waited 5×600ms = 3 s BEFORE bootstrapping → 3.8 s worst case.
    //
    // Empirically the active bootstrap is harmless on already-populated hosts
    // (Strategy A's enable_flutter_semantics just clicks a placeholder).
    let mut bootstrapped = false;
    for attempt in 0u8..5 {
        match shadow_find_once(role, name_regex) {
            Err(ref e) if e.contains("is empty") => {
                if attempt == 0 && !bootstrapped {
                    tracing::debug!(
                        "[shadow_find] host empty on cold call — actively bootstrapping Flutter semantics"
                    );
                    let _ = crate::browser_ax::enable_flutter_semantics();
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    bootstrapped = true;
                    continue;
                }
                if attempt < 4 {
                    tracing::debug!("[shadow_find] host still empty, poll {}/4 in 400ms", attempt + 1);
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    continue;
                }
                return shadow_find_once(role, name_regex); // surface real error
            }
            other => return other,
        }
    }
    shadow_find_once(role, name_regex)
}

fn shadow_find_once(role: Option<&str>, name_regex: Option<&str>) -> Result<(f64, f64, String), String> {
    if role.is_none() && name_regex.is_none() {
        return Err("shadow_find: need at least one of 'role' or 'name_regex'".into());
    }
    let role_val  = role.unwrap_or("").replace('\'', "\\'");
    let name_val  = name_regex.unwrap_or("").replace('\'', "\\'");

    let js = format!(r#"(() => {{
  // flt-semantics-host is a direct child of flutter-view, NOT inside flt-glass-pane shadow root.
  // Structure: body > flutter-view > flt-semantics-host > flt-semantics[role=...]
  const host = document.querySelector('flt-semantics-host');
  if (!host) {{
    const hasView = !!document.querySelector('flutter-view');
    return JSON.stringify({{ found: false, reason: 'flt-semantics-host not found (flutter-view present: ' + hasView + ')' }});
  }}
  if (host.childElementCount === 0) {{
    return JSON.stringify({{ found: false, reason: 'flt-semantics-host is empty (call enable_a11y first or wait for Flutter to build semantics)' }});
  }}
  const roleFilter   = '{role_val}';
  const namePattern  = '{name_val}';
  const re = namePattern ? new RegExp(namePattern, 'iu') : null;
  const sel = roleFilter ? '[role="' + roleFilter + '"]' : '[role]';
  const candidates = Array.from(host.querySelectorAll(sel));
  for (const el of candidates) {{
    if (re) {{
      // Flutter sets textContent on flt-semantics but aria-label is often null.
      // Check both: aria-label first, then textContent (trimmed).
      const lbl = el.getAttribute('aria-label') || el.textContent.trim() || '';
      if (!re.test(lbl)) continue;
    }}
    const r = el.getBoundingClientRect();
    if (r.width < 1 && r.height < 1) continue;
    // Prefer aria-label if present, fall back to textContent
    const label = el.getAttribute('aria-label') || el.textContent.trim() || '';
    return JSON.stringify({{
      found: true,
      x: r.left + r.width / 2,
      y: r.top  + r.height / 2,
      width: r.width, height: r.height,
      label: label,
      role:  el.getAttribute('role') || '',
    }});
  }}
  const avail = Array.from(host.querySelectorAll('[role]'))
    .map(e => (e.getAttribute('role')||'') + ':' + (e.getAttribute('aria-label') || e.textContent.trim() || ''))
    .slice(0, 30);
  return JSON.stringify({{ found: false, reason: 'no matching element', available: avail }});
}})()
"#, role_val = role_val, name_val = name_val);

    let raw = evaluate_js(&js)?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("shadow_find: JSON parse: {e} — raw={raw}"))?;

    if v.get("found").and_then(|f| f.as_bool()) != Some(true) {
        let reason = v["reason"].as_str().unwrap_or("unknown");
        let avail_arr = v.get("available").and_then(|a| a.as_array());

        // Fuzzy-suggest similar roles: when the requested name is matchable
        // under a DIFFERENT role, the LLM was likely picking the wrong one.
        // Empirically (run_..._0 batch 6) the LLM spent 4 iterations retrying
        // `shadow_click role=tab name=^商品$` without ever trying role=button —
        // pointing it at the right role one-shot saves the convergence-guard
        // abort and reaches the actual goal.
        let suggestion = avail_arr
            .map(|arr| suggest_role_from_available(name_regex, role, arr))
            .unwrap_or_default();

        let avail_str = avail_arr
            .map(|a| format!(", available: {}", serde_json::Value::Array(a.clone())))
            .unwrap_or_default();
        return Err(format!("shadow_find: {reason}{suggestion}{avail_str}"));
    }

    let x     = v["x"].as_f64().ok_or("shadow_find: missing x")?;
    let y     = v["y"].as_f64().ok_or("shadow_find: missing y")?;
    let label = v["label"].as_str().unwrap_or("").to_string();
    Ok((x, y, label))
}

/// Scan `available` (role:label list) for entries whose label matches the
/// requested `name_regex`, grouping by role.  Returns a hint string like
/// ` — try role=button (matches: "商品") or role=group` so the LLM can
/// switch role on its very next turn instead of grinding the convergence
/// guard.
///
/// Returns an empty string when no fuzzy match exists or when the requested
/// role itself is the only matching role (no useful suggestion to make).
fn suggest_role_from_available(
    name_regex: Option<&str>,
    requested_role: Option<&str>,
    available: &[serde_json::Value],
) -> String {
    let pattern = match name_regex {
        Some(p) if !p.is_empty() => p,
        _ => return String::new(),
    };
    // Strip regex anchors / common metas for fuzzy-substring comparison.
    let needle = pattern
        .trim_start_matches('^')
        .trim_end_matches('$')
        .to_lowercase();
    if needle.is_empty() || needle.len() > 80 {
        return String::new();
    }
    let mut matches_by_role: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for item in available {
        let s = match item.as_str() {
            Some(s) => s,
            None => continue,
        };
        // Items are "role:label" — split on first ':'.
        let (role, label) = match s.split_once(':') {
            Some(t) => t,
            None => continue,
        };
        if role.is_empty() || label.is_empty() {
            continue;
        }
        if label.to_lowercase().contains(&needle) {
            matches_by_role
                .entry(role.to_string())
                .or_default()
                .push(label.chars().take(20).collect());
        }
    }
    // Drop the requested role (LLM already tried it).
    if let Some(r) = requested_role {
        matches_by_role.remove(r);
    }
    if matches_by_role.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = matches_by_role
        .into_iter()
        .take(3) // cap suggestions so the error stays readable
        .map(|(role, labels)| {
            let sample = labels.into_iter().next().unwrap_or_default();
            format!("role={role} (e.g. \"{sample}\")")
        })
        .collect();
    parts.sort();
    format!(" — did you mean {}?", parts.join(" or "))
}

/// List all elements in Flutter's shadow DOM (debugging / inspection).
/// Returns a vec of `role:aria-label` strings.
pub fn shadow_dump() -> Result<Vec<String>, String> {
    let js = r#"(() => {
  const host = document.querySelector('flt-semantics-host');
  if (!host) return JSON.stringify(['ERROR:flt-semantics-host not found']);
  if (host.childElementCount === 0) return JSON.stringify(['EMPTY:call enable_a11y first']);
  return JSON.stringify(
    Array.from(host.querySelectorAll('[role]'))
      .map(e => (e.getAttribute('role')||'?') + ':' + (e.getAttribute('aria-label') || e.textContent.trim() || ''))
  );
})()
"#;
    let raw = evaluate_js(js)?;
    let v: Vec<String> = serde_json::from_str(&raw)
        .unwrap_or_else(|_| vec![format!("parse_error:{raw}")]);
    Ok(v)
}

/// Click an element found via Flutter's shadow DOM.
///
/// Uses JS `PointerEvent` dispatch directly on the `flt-semantics` element.
/// This is REQUIRED for Flutter CanvasKit: CDP `Input.dispatchMouseEvent`
/// (used by `click_point`) causes Chrome to navigate to `about:blank` on
/// certain Flutter route-change buttons (e.g. 質押 → /wallet/stake-form).
/// JS-dispatched pointer events are processed by Flutter's gesture recognizer
/// without side-effects.
///
/// Returns the aria-label / textContent of the clicked element.
pub fn shadow_click(role: Option<&str>, name_regex: Option<&str>) -> Result<String, String> {
    let role_val = role.unwrap_or("").replace('\'', "\\'");
    let name_val = name_regex.unwrap_or("").replace('\'', "\\'");

    let js = format!(r#"(() => {{
  const host = document.querySelector('flt-semantics-host');
  if (!host) return JSON.stringify({{ found: false, reason: 'no flt-semantics-host' }});
  if (host.childElementCount === 0) return JSON.stringify({{ found: false, reason: 'host empty' }});
  const roleFilter  = '{role_val}';
  const namePattern = '{name_val}';
  const re = namePattern ? new RegExp(namePattern, 'iu') : null;
  const sel = roleFilter ? '[role="' + roleFilter + '"]' : '[role]';
  const candidates = Array.from(host.querySelectorAll(sel));
  for (const el of candidates) {{
    if (re) {{
      const lbl = el.getAttribute('aria-label') || el.textContent.trim() || '';
      if (!re.test(lbl)) continue;
    }}
    const r = el.getBoundingClientRect();
    if (r.width < 1 && r.height < 1) continue;
    const cx = r.left + r.width / 2;
    const cy = r.top  + r.height / 2;
    const label = el.getAttribute('aria-label') || el.textContent.trim() || '';
    // Dispatch pointer events directly on the element.
    // CDP Input.dispatchMouseEvent causes about:blank on Flutter route-change
    // buttons; JS PointerEvent dispatch is handled correctly by Flutter.
    el.dispatchEvent(new PointerEvent('pointerdown', {{bubbles:true, cancelable:true, clientX:cx, clientY:cy, pointerId:1}}));
    el.dispatchEvent(new PointerEvent('pointerup',   {{bubbles:true, cancelable:true, clientX:cx, clientY:cy, pointerId:1}}));
    el.dispatchEvent(new PointerEvent('click',       {{bubbles:true, cancelable:true, clientX:cx, clientY:cy}}));
    return JSON.stringify({{ found: true, label: label, x: cx, y: cy }});
  }}
  const avail = Array.from(host.querySelectorAll('[role]'))
    .map(e => (e.getAttribute('role')||'') + ':' + (e.getAttribute('aria-label') || e.textContent.trim() || ''))
    .slice(0, 30);
  return JSON.stringify({{ found: false, reason: 'no matching element', available: avail }});
}})()
"#, role_val = role_val, name_val = name_val);

    let raw = evaluate_js(&js)?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("shadow_click: JSON parse: {e} — raw={raw}"))?;

    if v.get("found").and_then(|f| f.as_bool()) != Some(true) {
        let reason = v["reason"].as_str().unwrap_or("unknown");
        let avail  = v.get("available")
            .map(|a| format!(", available: {a}"))
            .unwrap_or_default();
        return Err(format!("shadow_click: {reason}{avail}"));
    }

    Ok(v["label"].as_str().unwrap_or("").to_string())
}

/// Focus + type into a Flutter shadow DOM element.
/// Uses JS pointer dispatch (same as shadow_click) to focus, then `Input.InsertText`.
pub fn shadow_type(role: Option<&str>, name_regex: Option<&str>, text: &str) -> Result<(), String> {
    // Use shadow_click (JS dispatch) to focus — click_point causes about:blank on Flutter
    shadow_click(role, name_regex)?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    with_tab(|tab| {
        tab.call_method(Input::InsertText { text: text.to_string() })
            .map_err(|e| format!("shadow_type InsertText: {e}"))
    })?;
    Ok(())
}

/// Type text into a Flutter text field by dispatching individual key presses.
///
/// Flutter Web's text editing engine listens for `keydown` / `keyup` events on
/// the `flt-text-editing-host` input — NOT for `Input.InsertText`.  This function
/// uses `tab.press_key()` (which sends proper CDP DispatchKeyEvent) so Flutter
/// receives and processes each character.
///
/// **CJK / non-ASCII support (Issue #143):** When `text` contains any non-ASCII
/// character the per-key path is bypassed because CDP `DispatchKeyEvent` has no
/// keycode for Unicode chars.  Instead we:
///   1. Try JS `ClipboardEvent('paste')` simulation — Flutter's text-editing
///      engine handles paste and updates `TextEditingController`.
///   2. Fall back to CDP `Input.InsertText` if the paste JS fails.
///
/// For mixed ASCII+CJK text the entire string is sent as a paste so both
/// scripts and the LLM can pass arbitrary Unicode without splitting the string.
///
/// Call `ax_click(backend_id)` or `shadow_click` first to focus the target field
/// and let Flutter create its `flt-text-editing` input; wait ~300 ms before calling
/// this function.
pub fn flutter_type(text: &str) -> Result<(), String> {
    let has_non_ascii = !text.is_ascii();

    // Clear existing content via JS — Ctrl+A + Delete does NOT work reliably in
    // Flutter Web because CDP Ctrl+A is processed as a literal 'a' character press.
    let _ = evaluate_js(
        "(() => { const inp = document.querySelector('.flt-text-editing'); if (inp) inp.value = ''; })()"
    );
    std::thread::sleep(std::time::Duration::from_millis(30));

    if has_non_ascii {
        return flutter_type_unicode(text);
    }

    // ASCII fast path: fire CDP keydown per character so Flutter's keydown handler fires.
    for ch in text.chars() {
        let key_str = ch.to_string();
        press_key(&key_str)
            .map_err(|e| format!("flutter_type '{key_str}': {e}"))?;
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Ok(())
}

/// Insert non-ASCII / CJK text into the currently-focused Flutter text field.
///
/// Strategy (two-stage):
///   1. JS clipboard paste simulation — Flutter Web's `EditableTextState`
///      responds to `ClipboardEvent('paste')` and forwards the text to the
///      `TextEditingController`.
///   2. CDP `Input.InsertText` fallback — works when the focused element is a
///      normal HTML input/textarea (non-Flutter or hybrid host).
///
/// This function is called automatically by `flutter_type` when the text
/// contains any non-ASCII character.  It can also be called directly.
/// Escape `text` for embedding in a single-quoted JS string literal.
/// Handles: backslash, single-quote, newline, carriage-return.
/// Kept as a separate fn so it can be unit-tested without a browser.
pub(crate) fn escape_for_js_single_quote(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

pub fn flutter_type_unicode(text: &str) -> Result<(), String> {
    // --- Stage 1: JS paste simulation ---
    // Escape the text for safe JS embedding (handles quotes, backslashes, etc.)
    let escaped = escape_for_js_single_quote(text);

    let js = format!(
        r#"(function() {{
            var text = '{escaped}';
            var inp = document.querySelector('.flt-text-editing');
            if (inp) {{
                // Method A: ClipboardEvent paste (preferred — Flutter handles this)
                try {{
                    var dt = new DataTransfer();
                    dt.setData('text/plain', text);
                    inp.dispatchEvent(new ClipboardEvent('paste', {{
                        clipboardData: dt,
                        bubbles: true,
                        cancelable: true
                    }}));
                    return 'paste_ok';
                }} catch(e) {{}}
                // Method B: InputEvent insertText (IME-like)
                try {{
                    inp.dispatchEvent(new InputEvent('input', {{
                        data: text,
                        inputType: 'insertText',
                        bubbles: true,
                        cancelable: true
                    }}));
                    return 'input_event_ok';
                }} catch(e) {{}}
            }}
            return 'no_input';
        }})()"#
    );

    let result = evaluate_js(&js)
        .unwrap_or_else(|e| format!("js_err:{e}"));

    if result.contains("paste_ok") || result.contains("input_event_ok") {
        // Give Flutter a tick to process the event
        std::thread::sleep(std::time::Duration::from_millis(80));
        return Ok(());
    }

    // --- Stage 2: CDP Input.InsertText fallback ---
    with_tab(|tab| {
        tab.call_method(Input::InsertText { text: text.to_string() })
            .map_err(|e| format!("flutter_type_unicode InsertText: {e}"))
    })?;
    std::thread::sleep(std::time::Duration::from_millis(80));
    Ok(())
}

/// Send the Enter key to the currently active Flutter text field.
///
/// Uses CDP `Input.DispatchKeyEvent` via `press_key("Return")` — this targets
/// whatever element currently has browser focus, so it works even after Flutter
/// removes the ephemeral `flt-text-editing` DOM element (which disappears a few
/// milliseconds after the last character is typed).
///
/// The old JS-based approach (`dispatchEvent` on `.flt-text-editing`) failed
/// whenever there was any delay between `flutter_type` and `flutter_enter`
/// because Flutter had already cleaned up its text editing element.
pub fn flutter_enter() -> Result<String, String> {
    press_key("Return").map_err(|e| format!("flutter_enter: {e}"))?;
    Ok(r#"{"sent":true}"#.to_string())
}

/// Scroll within a Flutter CanvasKit app by simulating a touch drag gesture.
///
/// `window.scrollBy` / wheel events don't move Flutter's internal scroll
/// controller — Flutter intercepts them before they can bubble.  Instead we
/// dispatch a `pointerdown → pointermove → pointerup` sequence (touch type)
/// on the Flutter glass pane, which Flutter's gesture recogniser translates
/// into a scroll delta.
///
/// `delta_y` > 0 scrolls DOWN (finger moves UP on screen).
/// `delta_y` < 0 scrolls UP.
///
/// Uses 8 interpolated `pointermove` steps to trigger Flutter's velocity
/// tracking; a single pointermove sometimes causes Flutter to treat the
/// gesture as a tap instead of a scroll.
/// Scroll within a Flutter CanvasKit app.
///
/// Tries two strategies in sequence, returning as soon as one works:
///
/// 1. **WheelEvent** — Flutter Web's `dart:html` listener handles `wheel`
///    events and translates them to scroll gestures.  This is the fastest
///    approach and works for most Flutter Scrollable widgets.
///
/// 2. **Touch-drag fallback** — dispatches `pointerdown → pointermove × 8 →
///    pointerup` on the glass pane.  Flutter's gesture recogniser translates
///    this into a drag-scroll delta.
///
/// `delta_y` > 0 scrolls DOWN, < 0 scrolls UP.
/// Both strategies dispatch to the centre of the viewport so the active
/// scrollable widget receives the event.
pub fn flutter_scroll(delta_y: f64) -> Result<(), String> {
    // Strategy 1: WheelEvent — preferred, lowest latency
    let js_wheel = format!(r#"(function() {{
    const target = document.querySelector('flt-glass-pane')
                || document.querySelector('canvas')
                || document.body;
    const cx = window.innerWidth  / 2;
    const cy = window.innerHeight / 2;
    for (let i = 0; i < 3; i++) {{
        target.dispatchEvent(new WheelEvent('wheel', {{
            bubbles: true, cancelable: true,
            clientX: cx, clientY: cy,
            deltaY: {delta_y} / 3,
            deltaMode: 0,
        }}));
    }}
    return 'wheel';
}})()"#);
    let _ = evaluate_js(&js_wheel);

    // Strategy 2: Touch-drag (8 pointermove steps for velocity tracking)
    let steps = 8i32;
    let step_y = delta_y / steps as f64;
    let js_touch = format!(r#"(function() {{
    const pane = document.querySelector('flt-glass-pane')
              || document.querySelector('canvas')
              || document.body;
    const cx = window.innerWidth  / 2;
    const cy = window.innerHeight / 2;
    const mk = (type, y) => new PointerEvent(type, {{
        bubbles: true, cancelable: true,
        clientX: cx, clientY: y,
        pointerId: 9, pointerType: 'touch', isPrimary: true,
    }});
    pane.dispatchEvent(mk('pointerdown', cy));
    for (let i = 1; i <= {steps}; i++) {{
        pane.dispatchEvent(mk('pointermove', cy - ({step_y} * i)));
    }}
    pane.dispatchEvent(mk('pointerup', cy - {delta_y}));
    return 'touch';
}})()"#);
    evaluate_js(&js_touch).map(|_| ())
}

/// Scroll within a Flutter CanvasKit app until a specific shadow DOM element
/// is visible, then stop.
///
/// Calls [`shadow_find`] after each scroll step to check whether the target
/// is now in the viewport (getBoundingClientRect() height > 0).  Returns as
/// soon as the element is found or `max_scroll_px` is exhausted.
///
/// `step_px` controls how many CSS pixels to scroll per iteration (default 300).
/// `max_scroll_px` caps the total scroll distance (default 2000).
///
/// Returns `Ok((x, y, label))` of the found element on success, or `Err` if
/// the element was not found within the scroll budget.
pub fn flutter_scroll_until_visible(
    role: Option<&str>,
    name_regex: Option<&str>,
    step_px: f64,
    max_scroll_px: f64,
) -> Result<(f64, f64, String), String> {
    // Check if the element is already visible before scrolling.
    if let Ok(hit) = shadow_find_in_viewport(role, name_regex) {
        return Ok(hit);
    }

    let step = if step_px <= 0.0 { 300.0 } else { step_px };
    let max  = if max_scroll_px <= 0.0 { 2000.0 } else { max_scroll_px };
    let mut scrolled = 0.0;

    while scrolled < max {
        flutter_scroll(step)?;
        scrolled += step;
        std::thread::sleep(std::time::Duration::from_millis(400));

        if let Ok(hit) = shadow_find_in_viewport(role, name_regex) {
            return Ok(hit);
        }
    }

    Err(format!(
        "flutter_scroll_until_visible: '{}' not found after scrolling {scrolled}px",
        name_regex.or(role).unwrap_or("?")
    ))
}

/// Like [`shadow_find`] but only returns elements that are currently inside
/// the viewport (getBoundingClientRect intersects window).
fn shadow_find_in_viewport(
    role: Option<&str>,
    name_regex: Option<&str>,
) -> Result<(f64, f64, String), String> {
    if role.is_none() && name_regex.is_none() {
        return Err("need role or name_regex".into());
    }
    let role_val = role.unwrap_or("").replace('\'', "\\'");
    let name_val = name_regex.unwrap_or("").replace('\'', "\\'");
    let js = format!(r#"(() => {{
  const host = document.querySelector('flt-semantics-host');
  if (!host || host.childElementCount === 0)
    return JSON.stringify({{ found: false, reason: 'host empty' }});
  const re = '{name_val}' ? new RegExp('{name_val}', 'iu') : null;
  const sel = '{role_val}' ? '[role="{role_val}"]' : '[role]';
  for (const el of host.querySelectorAll(sel)) {{
    const lbl = el.getAttribute('aria-label') || el.textContent.trim() || '';
    if (re && !re.test(lbl)) continue;
    const r = el.getBoundingClientRect();
    // Must be in viewport (height > 0 and top < window height)
    if (r.height < 1 || r.top >= window.innerHeight || r.bottom <= 0) continue;
    return JSON.stringify({{ found: true, x: r.left + r.width/2, y: r.top + r.height/2, label: lbl }});
  }}
  return JSON.stringify({{ found: false, reason: 'not in viewport' }});
}})()"#);
    let raw = evaluate_js(&js)?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("shadow_find_in_viewport parse: {e}"))?;
    if v["found"].as_bool() != Some(true) {
        return Err(v["reason"].as_str().unwrap_or("not found").to_string());
    }
    let x = v["x"].as_f64().ok_or("missing x")?;
    let y = v["y"].as_f64().ok_or("missing y")?;
    let label = v["label"].as_str().unwrap_or("").to_string();
    Ok((x, y, label))
}

/// Move the mouse to (x, y) **in CSS pixels** without clicking — triggers
/// hover effects.  See `click_point` for the CSS-vs-screenshot pixel caveat.
pub fn hover_point(x: f64, y: f64) -> Result<(), String> {
    with_tab(|tab| {
        // Point imported at module level
        tab.move_mouse_to_point(Point { x, y })
            .map_err(|e| format!("hover({x},{y}): {e}"))?;
        Ok(())
    })
}

/// Hover at a coordinate read from the screenshot PNG (physical pixels).
/// Auto-converts to CSS pixels using `window.devicePixelRatio`.  Issue #79.
pub fn hover_point_screenshot(px: f64, py: f64) -> Result<(), String> {
    let ctx = viewport_context().unwrap_or(ViewportContext {
        viewport_width: 0.0,
        viewport_height: 0.0,
        device_pixel_ratio: 1.0,
    });
    let (cx, cy) = ctx.screenshot_to_css(px, py);
    hover_point(cx, cy)
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
        inject_privacy_mask(tab);
        inject_hide_for_tool_use(tab);
        let res = tab.capture_screenshot(
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
        ).map_err(|e| format!("screenshot_element '{selector}': {e}"));
        remove_hide_for_tool_use(tab);
        remove_privacy_mask(tab);
        res
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
/// Sends CDP `Page.close` so Chrome actually removes the tab.
pub fn close_tab(index: usize) -> Result<(), String> {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_mut().ok_or("browser not open")?;
    if inner.tabs.len() <= 1 {
        return Err("cannot close the last tab".into());
    }
    if index >= inner.tabs.len() {
        return Err(format!("tab index {index} out of range"));
    }
    // Tell Chrome to actually close the tab.
    // Tab::close(fire_unload=false) — skip beforeunload to avoid dialog blocking.
    let _ = inner.tabs[index].close(false);
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

// ── Raw CDP helper for Storage.clearDataForOrigin ─────────────────────────────
// headless_chrome 1.0.x does not expose the Storage domain, so we use the
// same raw-method pattern as browser_ax::RawGetFullAxTree.
#[derive(Debug, serde::Serialize)]
struct RawClearDataForOrigin {
    origin: String,
    #[serde(rename = "storageTypes")]
    storage_types: String,
}
impl headless_chrome::protocol::cdp::types::Method for RawClearDataForOrigin {
    const NAME: &'static str = "Storage.clearDataForOrigin";
    type ReturnObject = serde_json::Value;
}

/// Wipe **all** storage (localStorage, sessionStorage, IndexedDB, cookies,
/// cache storage, service workers) for a given origin via CDP.
///
/// Does **not** require the browser to have navigated to that origin —
/// Chrome executes it against its profile database directly.  This is the
/// correct pre-navigate alternative to `clear_browser_state`, which runs JS
/// on the already-loaded page and therefore can't clear auth tokens that the
/// app has already read into memory.
///
/// Call this from the executor **before** `goto` so that Flutter sees empty
/// storage from frame zero and shows the login page instead of auto-logging in.
pub fn clear_origin_data(origin: &str) -> Result<(), String> {
    with_tab(|tab| {
        tab.call_method(RawClearDataForOrigin {
            origin: origin.to_string(),
            storage_types: "all".to_string(),
        })
        .map_err(|e| format!("Storage.clearDataForOrigin({origin}): {e}"))?;
        Ok(())
    })
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
            THREAD_ACTIVE_TAB.with(|c| c.set(Some(idx)));
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
    THREAD_ACTIVE_TAB.with(|c| c.set(Some(idx)));
    tracing::debug!("[browser] created session '{session_id}' → tab {idx}");
    Ok(idx)
}

/// Snapshot of the current Chrome browser state for MCP `browser_status`.
pub struct BrowserStatus {
    pub is_open:           bool,
    pub tab_count:         usize,
    pub active_tab_index:  usize,
    /// All tabs as (url) indexed by position.
    pub tabs:              Vec<String>,
    /// Named sessions: (session_id, tab_index, url).
    pub named_sessions:    Vec<(String, usize, String)>,
}

/// Return a non-failing snapshot of the browser state.
/// Safe to call even when Chrome is not running.
pub fn browser_status() -> BrowserStatus {
    let guard = global().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        None => BrowserStatus {
            is_open: false,
            tab_count: 0,
            active_tab_index: 0,
            tabs: vec![],
            named_sessions: vec![],
        },
        Some(inner) => {
            let tabs: Vec<String> = inner.tabs.iter()
                .map(|t| t.get_url())
                .collect();
            let mut named: Vec<(String, usize, String)> = inner.sessions.iter()
                .map(|(id, &idx)| {
                    let url = inner.tabs.get(idx).map(|t| t.get_url()).unwrap_or_default();
                    (id.clone(), idx, url)
                })
                .collect();
            named.sort_by_key(|(_, idx, _)| *idx);
            BrowserStatus {
                is_open: true,
                tab_count: tabs.len(),
                active_tab_index: inner.active,
                tabs,
                named_sessions: named,
            }
        }
    }
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
/// Sends CDP `Page.close` so Chrome actually removes the tab.
pub fn close_session(session_id: &str) -> Result<(), String> {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    let inner = guard.as_mut().ok_or("browser not open")?;
    let idx = inner.sessions.remove(session_id)
        .ok_or_else(|| format!("session '{session_id}' not found"))?;
    if inner.tabs.len() <= 1 {
        return Err("cannot close the last tab".into());
    }
    if idx < inner.tabs.len() {
        // Tell Chrome to actually close the tab via Tab::close().
        let _ = inner.tabs[idx].close(false);
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

    // ── Clone Arc<Tab> under short lock, then release before the CDP call ──
    // Previously the lock was held for the full duration of f(), which caused
    // deadlocks when Chrome crashed mid-run: the hung CDP call kept the mutex
    // locked, preventing all concurrent callers from checking or resetting the
    // singleton.  Cloning the Arc<Tab> and dropping the guard first means:
    //   • other threads can still acquire the lock to clear a dead session;
    //   • the Tab object stays alive (Arc refcount ≥ 1) so the CDP call
    //     proceeds safely on the cloned reference.
    //
    // Thread-local tab index: `session_switch` writes THREAD_ACTIVE_TAB so
    // each concurrent test thread always reads its own tab, not the shared
    // `inner.active` pointer that any other thread may have clobbered.
    let tab: Arc<Tab> = {
        let guard = global().lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(inner) => {
                let idx = THREAD_ACTIVE_TAB.with(|c| c.get())
                    .unwrap_or(inner.active)
                    .min(inner.tabs.len().saturating_sub(1));
                Arc::clone(&inner.tabs[idx])
            }
            None => return Err("browser session lost".into()),
        }
    }; // mutex released here — NOT held during the CDP call

    let mut result = f.clone()(&tab);

    // If the call failed with a connection-closed error, retry up to
    // `MAX_RECOVERIES` times.  Pilot #003-rerun (Issue #97) showed
    // `agora_pickup_time_picker` hitting `net::ERR_ABORTED` repeatedly on
    // Flutter hash-route transitions — one-shot recovery isn't always enough
    // when the underlying CDP transport is genuinely flapping.  Each retry
    // resets the singleton + sleeps briefly to let Chrome settle.
    const MAX_RECOVERIES: u32 = 3;
    for attempt in 1..=MAX_RECOVERIES {
        match &result {
            Err(e) if is_connection_closed(e) => {
                tracing::warn!(
                    "[browser] mid-call connection closed (attempt {}/{}) — recovering",
                    attempt, MAX_RECOVERIES
                );
                // Clear singleton so the next ensure_open spawns a fresh Chrome.
                *global().lock().unwrap_or_else(|e| e.into_inner()) = None;
                // Brief settle delay between recoveries — empirically the second+
                // recovery succeeds when Chrome had a moment to release its locks.
                std::thread::sleep(std::time::Duration::from_millis(500));
                // Re-launch in the mode the current test requested (not
                // default_headless()).  Flutter CanvasKit needs headless=false
                // — using default would silently re-launch headless and cause
                // a black screen for the rest of the test.
                let recovery_headless = TEST_DESIRED_HEADLESS.load(Ordering::Relaxed);
                if let Err(launch_err) = ensure_open(recovery_headless) {
                    tracing::error!(
                        "[browser] recovery attempt {} failed at ensure_open: {}",
                        attempt, launch_err
                    );
                    if attempt == MAX_RECOVERIES {
                        return Err(format!(
                            "browser recovery exhausted after {} attempts: {}",
                            MAX_RECOVERIES, launch_err
                        ));
                    }
                    continue;
                }
                // Get a fresh tab from the new session.  After recovery the
                // browser has only one tab (index 0) — clamp THREAD_ACTIVE_TAB
                // so we don't panic on out-of-bounds.
                let tab: Arc<Tab> = {
                    let guard = global().lock().unwrap_or_else(|e| e.into_inner());
                    match guard.as_ref() {
                        Some(inner) => {
                            let idx = THREAD_ACTIVE_TAB.with(|c| c.get())
                                .unwrap_or(inner.active)
                                .min(inner.tabs.len().saturating_sub(1));
                            Arc::clone(&inner.tabs[idx])
                        }
                        None => {
                            if attempt == MAX_RECOVERIES {
                                return Err("browser session lost after recovery".into());
                            }
                            continue;
                        }
                    }
                };
                result = f.clone()(&tab);
            }
            _ => break, // Ok, or non-connection-closed Err — stop retrying.
        }
    }

    result
}

fn is_connection_closed(err: &str) -> bool {
    err.contains("underlying connection is closed")
        || err.contains("TaskCancelled")
        || err.contains("ChannelClosed")
        // CDP transport-layer timeouts (headless_chrome phrases):
        || err.contains("event waited for never came")   // wait_until_navigated / wait_for_element
        || err.contains("timed out")                     // generic CDP timeout
        || err.contains("transport loop")                // WebSocket transport died
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

    // ── Issue #143: flutter_type_unicode JS escape helper ─────────────────────

    #[test]
    fn js_escape_plain_ascii_unchanged() {
        assert_eq!(escape_for_js_single_quote("hello"), "hello");
        assert_eq!(escape_for_js_single_quote("12345"), "12345");
    }

    #[test]
    fn js_escape_backslash_doubled() {
        assert_eq!(escape_for_js_single_quote("a\\b"), "a\\\\b");
    }

    #[test]
    fn js_escape_single_quote_escaped() {
        assert_eq!(escape_for_js_single_quote("it's"), "it\\'s");
    }

    #[test]
    fn js_escape_newline_and_cr_escaped() {
        assert_eq!(escape_for_js_single_quote("a\nb"), "a\\nb");
        assert_eq!(escape_for_js_single_quote("a\rb"), "a\\rb");
        assert_eq!(escape_for_js_single_quote("a\r\nb"), "a\\r\\nb");
    }

    #[test]
    fn js_escape_cjk_passthrough() {
        // CJK chars don't need escaping — verify they survive unchanged
        let chinese = "你好世界";
        assert_eq!(escape_for_js_single_quote(chinese), chinese);
        let thai = "สวัสดี";
        assert_eq!(escape_for_js_single_quote(thai), thai);
    }

    #[test]
    fn js_escape_mixed_content() {
        let input = "O'Brien\n\\path\\";
        let expected = "O\\'Brien\\n\\\\path\\\\";
        assert_eq!(escape_for_js_single_quote(input), expected);
    }

    #[test]
    fn flutter_type_detects_non_ascii() {
        // has_non_ascii detection: any non-ASCII char triggers the unicode path.
        // Since we can't call flutter_type without a browser, we test the
        // detection predicate directly.
        assert!("你好".chars().any(|c| !c.is_ascii()), "CJK should be non-ASCII");
        assert!("สวัสดี".chars().any(|c| !c.is_ascii()), "Thai should be non-ASCII");
        assert!(!"hello".chars().any(|c| !c.is_ascii()), "pure ASCII should NOT trigger");
        assert!(!"abc123".chars().any(|c| !c.is_ascii()), "alphanum is ASCII");
        // Mixed: even one CJK triggers the path
        assert!("hello 你".chars().any(|c| !c.is_ascii()), "mixed should trigger");
    }

    // ── Issue #79: HiDPI screenshot↔CSS pixel conversion ──────────────────────

    // ── Issue #74: dom_snapshot + ref_id helpers ─────────────────────────────

    #[test]
    fn ref_id_validates_eN_schema() {
        assert!(is_valid_ref_id("e1"));
        assert!(is_valid_ref_id("e42"));
        assert!(is_valid_ref_id("e9999"));
        // Invalid forms
        assert!(!is_valid_ref_id(""));
        assert!(!is_valid_ref_id("e"));
        assert!(!is_valid_ref_id("ref_1"));        // CiC-style not accepted
        assert!(!is_valid_ref_id("E1"));           // case-sensitive
        assert!(!is_valid_ref_id("e1a"));          // trailing non-digit
        assert!(!is_valid_ref_id("1e"));
        assert!(!is_valid_ref_id("e-1"));
        // Selector-injection attempts must NOT validate.
        assert!(!is_valid_ref_id(r#"e1"]"#));
        assert!(!is_valid_ref_id("e1; drop"));
    }

    #[test]
    fn ref_selector_quotes_attribute_consistently() {
        assert_eq!(ref_selector("e1"), r#"[data-sirin-ref="e1"]"#);
        assert_eq!(ref_selector("e42"), r#"[data-sirin-ref="e42"]"#);
        // Selector must be a valid CSS attr-equals, idempotent,
        // and round-trippable through is_valid_ref_id for the inner id.
        let sel = ref_selector("e7");
        assert!(sel.starts_with("[data-sirin-ref="));
        assert!(sel.ends_with("\"]"));
    }

    #[test]
    fn dom_snapshot_js_embeds_max_and_returns_callable_iife() {
        // Pure unit test of the JS payload generator — no Chrome.
        let js = dom_snapshot_js(50);
        assert!(js.starts_with("(function(){"));
        assert!(js.trim_end().ends_with("})()"));
        assert!(js.contains("var MAX = 50"), "MAX must be embedded literally");
        assert!(js.contains("__sirinRefMap"));
        assert!(js.contains("data-sirin-ref"));
        // No leftover Rust-format placeholders.
        assert!(!js.contains("{max}"), "format placeholder leaked into JS");
        assert!(!js.contains("{{") || js.contains("{{a:'link'"), "literal braces only inside object literals");

        // Default cap.
        let big = dom_snapshot_js(DOM_SNAPSHOT_DEFAULT_MAX);
        assert!(big.contains(&format!("var MAX = {}", DOM_SNAPSHOT_DEFAULT_MAX)));
    }

    #[test]
    fn viewport_ctx_dpr_2x_screenshot_to_css_halves_coords() {
        let ctx = ViewportContext {
            viewport_width: 1920.0,
            viewport_height: 1080.0,
            device_pixel_ratio: 2.0,
        };
        // A point at the bottom-right of a 2× screenshot (3840 × 2160) maps to
        // the bottom-right of the 1920 × 1080 CSS viewport.
        assert_eq!(ctx.screenshot_to_css(3840.0, 2160.0), (1920.0, 1080.0));
        // Sub-pixel midpoints survive the divide.
        assert_eq!(ctx.screenshot_to_css(1280.0, 840.0), (640.0, 420.0));
    }

    #[test]
    fn viewport_ctx_dpr_1x_is_identity() {
        let ctx = ViewportContext {
            viewport_width: 1366.0,
            viewport_height: 768.0,
            device_pixel_ratio: 1.0,
        };
        assert_eq!(ctx.screenshot_to_css(640.0, 420.0), (640.0, 420.0));
    }

    #[test]
    fn viewport_ctx_zero_or_negative_dpr_falls_back_to_1() {
        // Defensive: a buggy probe returning 0 should not divide-by-zero or
        // flip the coords — fall back to identity.
        let ctx = ViewportContext {
            viewport_width: 100.0,
            viewport_height: 100.0,
            device_pixel_ratio: 0.0,
        };
        assert_eq!(ctx.screenshot_to_css(50.0, 50.0), (50.0, 50.0));
    }

    #[test]
    fn default_viewport_falls_back_when_env_unset() {
        std::env::remove_var("SIRIN_DEFAULT_VIEWPORT");
        assert_eq!(resolve_default_viewport(), (1440, 1600));
    }

    #[test]
    fn default_viewport_parses_wxh_form() {
        std::env::set_var("SIRIN_DEFAULT_VIEWPORT", "1920x1200");
        assert_eq!(resolve_default_viewport(), (1920, 1200));

        std::env::set_var("SIRIN_DEFAULT_VIEWPORT", "1280X720");
        assert_eq!(resolve_default_viewport(), (1280, 720));

        std::env::set_var("SIRIN_DEFAULT_VIEWPORT", "800,600");
        assert_eq!(resolve_default_viewport(), (800, 600));

        std::env::remove_var("SIRIN_DEFAULT_VIEWPORT");
    }

    #[test]
    fn default_viewport_clamps_absurd_values() {
        std::env::set_var("SIRIN_DEFAULT_VIEWPORT", "100x100");
        let (w, h) = resolve_default_viewport();
        assert!(w >= 640 && h >= 480);

        std::env::set_var("SIRIN_DEFAULT_VIEWPORT", "99999x99999");
        let (w, h) = resolve_default_viewport();
        assert!(w <= 3840 && h <= 4320);

        std::env::remove_var("SIRIN_DEFAULT_VIEWPORT");
    }

    #[test]
    fn default_viewport_rejects_garbage() {
        std::env::set_var("SIRIN_DEFAULT_VIEWPORT", "nonsense");
        assert_eq!(resolve_default_viewport(), (1440, 1600));

        std::env::set_var("SIRIN_DEFAULT_VIEWPORT", "1440");
        assert_eq!(resolve_default_viewport(), (1440, 1600));

        std::env::remove_var("SIRIN_DEFAULT_VIEWPORT");
    }

    #[test]
    fn heartbeat_interval_safely_under_cdp_timeout() {
        // headless_chrome's transport loop dies after ~30 s of CDP silence.
        // If we ever bump HEARTBEAT_INTERVAL_SECS too close to 30, one
        // dropped pulse kills the connection.  Keep a healthy margin.
        assert!(
            HEARTBEAT_INTERVAL_SECS <= 20,
            "heartbeat interval {HEARTBEAT_INTERVAL_SECS}s is too close to \
             the 30s CDP transport timeout — a single missed pulse would die"
        );
        assert!(HEARTBEAT_INTERVAL_SECS >= 5, "heartbeat too chatty");
    }

    #[test]
    fn persistent_profile_env_off_returns_none() {
        // Use unsafe env-manipulation carefully — test runs sequentially
        // because we touch a process-wide env var.
        std::env::remove_var("SIRIN_PERSISTENT_PROFILE");
        assert!(resolve_persistent_profile_dir().is_none());

        std::env::set_var("SIRIN_PERSISTENT_PROFILE", "");
        assert!(resolve_persistent_profile_dir().is_none());

        std::env::set_var("SIRIN_PERSISTENT_PROFILE", "0");
        assert!(resolve_persistent_profile_dir().is_none());

        std::env::set_var("SIRIN_PERSISTENT_PROFILE", "false");
        assert!(resolve_persistent_profile_dir().is_none());

        std::env::remove_var("SIRIN_PERSISTENT_PROFILE");
    }

    #[test]
    fn persistent_profile_env_truthy_uses_default_dir() {
        std::env::set_var("SIRIN_PERSISTENT_PROFILE", "1");
        let dir = resolve_persistent_profile_dir().expect("truthy → Some(path)");
        assert!(dir.ends_with("chrome-profile"));
        std::env::remove_var("SIRIN_PERSISTENT_PROFILE");
    }

    #[test]
    fn persistent_profile_env_custom_path_passes_through() {
        let tmp = std::env::temp_dir().join("sirin-test-profile-dir");
        std::env::set_var("SIRIN_PERSISTENT_PROFILE", &tmp);
        let dir = resolve_persistent_profile_dir().expect("path → Some(path)");
        assert_eq!(dir, tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("SIRIN_PERSISTENT_PROFILE");
    }

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
    fn privacy_mask_default_on_and_toggle_round_trips() {
        // Default = on (fail-secure) — explicitly set so we don't depend on
        // sibling-test side effects.
        let _ = set_privacy_mask(true);
        assert!(privacy_mask_enabled(), "default must be on for fail-secure");

        let prev = set_privacy_mask(false);
        assert!(prev, "set_privacy_mask returns previous value");
        assert!(!privacy_mask_enabled());

        let prev = set_privacy_mask(true);
        assert!(!prev);
        assert!(privacy_mask_enabled());
    }

    #[test]
    fn privacy_mask_env_off_disables_default() {
        let orig = std::env::var("SIRIN_PRIVACY_MASK").ok();

        // Anything but explicit off → on (fail-secure)
        std::env::set_var("SIRIN_PRIVACY_MASK", "1");
        init_privacy_mask_from_env();
        assert!(privacy_mask_enabled());

        std::env::remove_var("SIRIN_PRIVACY_MASK");
        init_privacy_mask_from_env();
        assert!(privacy_mask_enabled(), "unset must default to on");

        std::env::set_var("SIRIN_PRIVACY_MASK", "garbage");
        init_privacy_mask_from_env();
        assert!(privacy_mask_enabled(), "unrecognised must default to on");

        // Explicit off variants
        for off in ["0", "false", "FALSE", "no", "NO"] {
            std::env::set_var("SIRIN_PRIVACY_MASK", off);
            init_privacy_mask_from_env();
            assert!(!privacy_mask_enabled(), "{off} must disable mask");
        }

        // Restore
        match orig {
            Some(v) => std::env::set_var("SIRIN_PRIVACY_MASK", v),
            None    => std::env::remove_var("SIRIN_PRIVACY_MASK"),
        }
        // Leave global in fail-secure state for subsequent tests
        let _ = set_privacy_mask(true);
    }

    #[test]
    fn action_indicator_toggle_round_trips() {
        // Default = off (CI safety).
        let _ = set_action_indicator(false);
        assert!(!action_indicator_enabled(), "default must be off");

        let prev = set_action_indicator(true);
        assert!(!prev, "set_action_indicator returns previous value");
        assert!(action_indicator_enabled());

        let prev = set_action_indicator(false);
        assert!(prev);
        assert!(!action_indicator_enabled());
    }

    #[test]
    fn action_indicator_inject_js_escapes_action_label() {
        // Backticks and backslashes in action labels must not break out of
        // the JS template literal.  We simply check the produced JS contains
        // the escaped form rather than the raw form.
        let js = build_indicator_inject_js("click `evil` \\path");
        assert!(js.contains(r"\`evil\`"), "backticks must be escaped: {js}");
        assert!(js.contains(r"\\path"), "backslashes must be escaped: {js}");
        // Stable IDs present so AX/screenshot filter can match them
        assert!(js.contains(ACTION_INDICATOR_BORDER_ID));
        assert!(js.contains(ACTION_INDICATOR_BADGE_ID));
    }

    #[test]
    fn hide_for_tool_use_css_hides_indicator_and_data_attrs() {
        // Sanity-check that the hide CSS actually targets:
        //   1. The indicator border + badge (Sirin's own UI).
        //   2. Both `data-sirin-hide` and the CiC-compatible `data-claude-hide`.
        for needle in [
            "data-sirin-hide",
            "data-claude-hide",
            ACTION_INDICATOR_BORDER_ID,
            ACTION_INDICATOR_BADGE_ID,
            "visibility: hidden",
        ] {
            assert!(
                HIDE_FOR_TOOL_USE_CSS.contains(needle),
                "hide CSS missing '{needle}': {}",
                HIDE_FOR_TOOL_USE_CSS
            );
        }
    }

    #[test]
    fn privacy_mask_css_covers_known_sensitive_selectors() {
        // Sanity-check that the CSS we will inject actually targets the
        // selectors enumerated in Issue #80.  Catches accidental deletions.
        for sel in [
            r#"input[type="password"]"#,
            r#"input[autocomplete*="cc-"]"#,
            r#"input[autocomplete*="one-time-code"]"#,
            r#"input[name*="password" i]"#,
            r#"input[name*="ssn" i]"#,
            r#"input[name*="cardnumber" i]"#,
            r#"input[aria-label*="password" i]"#,
            r#"[data-sensitive]"#,
        ] {
            assert!(
                PRIVACY_MASK_CSS.contains(sel),
                "PRIVACY_MASK_CSS missing selector: {sel}"
            );
        }
        // Mask must visually destroy the value, not just blur (blur alone
        // can be reversed by sharpening filters in vision LLMs).
        assert!(PRIVACY_MASK_CSS.contains("filter: blur(8px)"));
        assert!(PRIVACY_MASK_CSS.contains("color: transparent"));
        assert!(PRIVACY_MASK_CSS.contains("background:"));

        // The injection script must reference the same style id we remove.
        assert!(build_inject_js().contains(PRIVACY_MASK_STYLE_ID));
        assert!(build_remove_js().contains(PRIVACY_MASK_STYLE_ID));
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

    // ── shadow_find fuzzy role suggestion (pure unit, no Chrome) ─────────────

    fn vstr(s: &str) -> serde_json::Value {
        serde_json::Value::String(s.to_string())
    }

    /// The canonical scenario from run_..._0 batch 6: LLM asked role=tab
    /// name=^商品$ but the page only has button:商品 — suggestion should
    /// point at role=button so the LLM can switch on its next turn.
    #[test]
    fn suggest_role_points_at_correct_role_when_label_matches_under_other_role() {
        let avail = vec![
            vstr("button:商品"),
            vstr("button:首頁"),
            vstr("group:導覽"),
            vstr("tab:全部"),
            vstr("tab:上架"),
        ];
        let s = suggest_role_from_available(Some("^商品$"), Some("tab"), &avail);
        assert!(s.contains("role=button"), "got: {s}");
        assert!(s.contains("商品"), "got: {s}");
    }

    /// When matches exist under several roles we should suggest each (capped
    /// at 3) — sorted so output is deterministic across runs.
    #[test]
    fn suggest_role_lists_multiple_alternatives_capped_at_3() {
        let avail = vec![
            vstr("button:商品列表"),
            vstr("link:商品分類"),
            vstr("group:商品庫存"),
            vstr("heading:商品首頁"),
            vstr("region:商品區"),
        ];
        let s = suggest_role_from_available(Some("商品"), Some("tab"), &avail);
        // Cap = 3
        let role_count = s.matches("role=").count();
        assert!(role_count <= 3, "got {role_count} role suggestions in: {s}");
        assert!(role_count >= 1, "expected at least one suggestion: {s}");
    }

    /// When no available element labels match the requested name, return
    /// empty string — don't add noise to the error message.
    #[test]
    fn suggest_role_empty_when_no_label_matches() {
        let avail = vec![vstr("button:訂單"), vstr("tab:全部")];
        assert_eq!(
            suggest_role_from_available(Some("^商品$"), Some("tab"), &avail),
            ""
        );
    }

    /// If the LLM asks role=tab and the only matches are also role=tab, we
    /// have nothing useful to suggest (the LLM already tried this role).
    #[test]
    fn suggest_role_skips_requested_role_in_suggestions() {
        let avail = vec![vstr("tab:商品上架"), vstr("tab:商品下架")];
        let s = suggest_role_from_available(Some("商品"), Some("tab"), &avail);
        assert_eq!(s, "", "got: {s}");
    }

    /// Empty / missing name_regex means we don't have a needle to match
    /// against — return empty rather than guess.
    #[test]
    fn suggest_role_empty_when_name_pattern_is_empty() {
        let avail = vec![vstr("button:商品")];
        assert_eq!(suggest_role_from_available(None, Some("tab"), &avail), "");
        assert_eq!(suggest_role_from_available(Some(""), Some("tab"), &avail), "");
    }

    /// Anchors `^` and `$` are stripped so the substring fuzzy compare works
    /// against e.g. "商品列表" when the LLM asked `^商品$`.
    #[test]
    fn suggest_role_strips_regex_anchors_for_fuzzy_match() {
        let avail = vec![vstr("button:商品列表")];
        let s = suggest_role_from_available(Some("^商品$"), Some("tab"), &avail);
        assert!(s.contains("role=button"), "got: {s}");
    }

    /// The label in the suggestion is truncated to 20 chars so very long
    /// labels (e.g. paginated card titles) don't blow up the error string.
    #[test]
    fn suggest_role_truncates_long_labels() {
        let long = format!("group:{}", "a".repeat(200));
        let avail = vec![vstr(&long)];
        let s = suggest_role_from_available(Some("aaa"), Some("tab"), &avail);
        // Label snippet should not exceed ~25 chars (20 + quotes + role).
        assert!(s.contains("role=group"), "got: {s}");
        let snippet = s.split('"').nth(1).unwrap_or("");
        assert!(snippet.len() <= 20, "label snippet too long ({}): {s}", snippet.len());
    }
}
