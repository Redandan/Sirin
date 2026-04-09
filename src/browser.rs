//! Headless-browser session wrapping `headless_chrome`.
//!
//! All operations are synchronous from the caller's perspective — use
//! `tokio::task::spawn_blocking` when calling from async code.

use headless_chrome::{Browser, LaunchOptions, Tab};
use std::sync::Arc;

pub struct BrowserSession {
    _browser: Browser,
    tab: Arc<Tab>,
}

impl BrowserSession {
    /// Launch a new headless Chrome instance and return a session with one open tab.
    pub fn launch() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let browser = Browser::new(
            LaunchOptions::default_builder()
                .headless(true)
                .build()
                .map_err(|e| format!("LaunchOptions build failed: {e}"))?,
        )?;
        let tab = browser.new_tab()?;
        Ok(Self { _browser: browser, tab })
    }

    /// Navigate to a URL and wait for the page to load.
    pub fn navigate(&self, url: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.tab.navigate_to(url)?.wait_until_navigated()?;
        Ok(())
    }



    /// Capture a full-page PNG screenshot and return the raw bytes.
    pub fn screenshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
        let png = self
            .tab
            .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)?;
        Ok(png)
    }

    /// Convenience: navigate then screenshot in one call.
    pub fn navigate_and_screenshot(
        url: &str,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let session = Self::launch()?;
        session.navigate(url)?;
        session.screenshot()
    }
}
