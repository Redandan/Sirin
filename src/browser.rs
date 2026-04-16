//! Persistent browser session wrapping `headless_chrome`.
//!
//! ## Concurrency
//! A process-wide singleton (`SESSION`) holds the active browser.  All public
//! helpers acquire the inner `Mutex` for the duration of a single CDP call,
//! keeping lock contention short.  Call from async code via
//! `tokio::task::spawn_blocking`.

use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
use headless_chrome::{Browser, LaunchOptions, Tab};
use std::sync::{Arc, Mutex, OnceLock};

// ── Singleton ────────────────────────────────────────────────────────────────

static SESSION: OnceLock<Arc<Mutex<Option<BrowserInner>>>> = OnceLock::new();

fn global() -> &'static Arc<Mutex<Option<BrowserInner>>> {
    SESSION.get_or_init(|| Arc::new(Mutex::new(None)))
}

struct BrowserInner {
    _browser: Browser,
    tab: Arc<Tab>,
    #[allow(dead_code)]
    headless: bool,
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Ensure a persistent browser is running.  If one already exists, this is a
/// no-op.  Returns `true` if a new browser was launched.
pub fn ensure_open(headless: bool) -> Result<bool, String> {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return Ok(false);
    }
    let opts = LaunchOptions::default_builder()
        .headless(headless)
        .build()
        .map_err(|e| format!("LaunchOptions: {e}"))?;
    let browser = Browser::new(opts).map_err(|e| format!("Browser::new: {e}"))?;
    let tab = browser.new_tab().map_err(|e| format!("new_tab: {e}"))?;
    *guard = Some(BrowserInner { _browser: browser, tab, headless });
    Ok(true)
}

/// Whether a browser session is currently alive.
pub fn is_open() -> bool {
    global()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// Whether the session is headless.
#[allow(dead_code)]
pub fn is_headless() -> Option<bool> {
    global()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|s| s.headless)
}

/// Close the browser and release resources.
pub fn close() {
    let mut guard = global().lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

// ── Navigation ───────────────────────────────────────────────────────────────

/// Navigate to a URL.  Launches headless if no session exists.
pub fn navigate(url: &str) -> Result<(), String> {
    with_tab(|tab| {
        tab.navigate_to(url)
            .map_err(|e| format!("navigate: {e}"))?
            .wait_until_navigated()
            .map_err(|e| format!("wait: {e}"))?;
        Ok(())
    })
}

/// Capture a PNG screenshot of the current page.
pub fn screenshot() -> Result<Vec<u8>, String> {
    with_tab(|tab| {
        tab.capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
            .map_err(|e| format!("screenshot: {e}"))
    })
}

/// Navigate + screenshot convenience (auto-launches headless).
#[allow(dead_code)]
pub fn navigate_and_screenshot(url: &str) -> Result<Vec<u8>, String> {
    ensure_open(true)?;
    navigate(url)?;
    screenshot()
}

/// Get the current page URL.
pub fn current_url() -> Result<String, String> {
    with_tab(|tab| Ok(tab.get_url()))
}

/// Get the page <title>.
pub fn page_title() -> Result<String, String> {
    with_tab(|tab| tab.get_title().map_err(|e| format!("title: {e}")))
}

// ── DOM Interaction ──────────────────────────────────────────────────────────

/// Click the first element matching a CSS selector.
pub fn click(selector: &str) -> Result<(), String> {
    with_tab(|tab| {
        let el = tab
            .wait_for_element(selector)
            .map_err(|e| format!("click – find '{selector}': {e}"))?;
        el.click().map_err(|e| format!("click '{selector}': {e}"))?;
        Ok(())
    })
}

/// Focus an element and type text into it.
pub fn type_text(selector: &str, text: &str) -> Result<(), String> {
    with_tab(|tab| {
        let el = tab
            .wait_for_element(selector)
            .map_err(|e| format!("type – find '{selector}': {e}"))?;
        el.click().map_err(|e| format!("type – focus '{selector}': {e}"))?;
        el.type_into(text)
            .map_err(|e| format!("type_into '{selector}': {e}"))?;
        Ok(())
    })
}

/// Read innerText of the first element matching a selector.
pub fn get_text(selector: &str) -> Result<String, String> {
    with_tab(|tab| {
        let el = tab
            .wait_for_element(selector)
            .map_err(|e| format!("get_text – find '{selector}': {e}"))?;
        el.get_inner_text()
            .map_err(|e| format!("get_inner_text '{selector}': {e}"))
    })
}

/// Evaluate arbitrary JavaScript and return the string result.
pub fn evaluate_js(expression: &str) -> Result<String, String> {
    with_tab(|tab| {
        let remote_obj = tab
            .evaluate(expression, true)
            .map_err(|e| format!("evaluate: {e}"))?;
        // Extract the value as a string (RemoteObject.value is Option<Value>).
        match remote_obj.value {
            Some(v) => Ok(match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            }),
            None => Ok(remote_obj
                .description
                .unwrap_or_else(|| "undefined".into())),
        }
    })
}

/// Get full page HTML content.
#[allow(dead_code)]
pub fn get_content() -> Result<String, String> {
    with_tab(|tab| {
        tab.get_content().map_err(|e| format!("get_content: {e}"))
    })
}

// ── Internals ────────────────────────────────────────────────────────────────

/// Run a closure with the active `Tab`.  Auto-launches headless if needed.
fn with_tab<F, R>(f: F) -> Result<R, String>
where
    F: FnOnce(&Arc<Tab>) -> Result<R, String>,
{
    ensure_open(true)?;
    let guard = global().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(inner) => f(&inner.tab),
        None => Err("browser session lost".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full lifecycle: launch → navigate → screenshot → read → eval → title → close.
    /// Requires Chrome installed. Run: cargo test --bin sirin browser_lifecycle -- --ignored
    #[test]
    #[ignore] // needs Chrome
    fn browser_lifecycle() {
        // 1) Launch headless
        assert!(!is_open());
        ensure_open(true).expect("launch");
        assert!(is_open());

        // 2) Navigate to a data URI (no network needed)
        let html = "data:text/html,<html><head><title>SirinTest</title></head><body><h1 id='msg'>Hello</h1><input id='box'/></body></html>";
        navigate(html).expect("navigate");

        // 3) Screenshot
        let png = screenshot().expect("screenshot");
        assert!(png.len() > 100, "png too small: {} bytes", png.len());

        // 4) Read element text
        let text = get_text("#msg").expect("get_text");
        assert_eq!(text.trim(), "Hello");

        // 5) Evaluate JS
        let title = evaluate_js("document.title").expect("eval");
        assert_eq!(title, "SirinTest");

        // 6) Page title helper
        let title2 = page_title().expect("page_title");
        assert_eq!(title2, "SirinTest");

        // 7) Current URL
        let url = current_url().expect("url");
        assert!(url.starts_with("data:"), "url was: {url}");

        // 8) Click (h1 is clickable, just verifying no error)
        click("#msg").expect("click h1");

        // 9) Type into input
        type_text("#box", "Sirin").expect("type_text");
        let typed = evaluate_js("document.getElementById('box').value").expect("read input");
        assert_eq!(typed, "Sirin");

        // 10) Close
        close();
        assert!(!is_open());

        println!("✓ browser_lifecycle: all 10 steps passed");
    }
}
