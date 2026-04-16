//! `BrowserService` impl — delegates to the `crate::browser` singleton.

use super::RealService;

pub fn browser_is_open(_svc: &RealService) -> bool {
    crate::browser::is_open()
}

pub fn browser_open(_svc: &RealService, url: &str, headless: bool) {
    if let Err(e) = crate::browser::ensure_open(headless) {
        tracing::error!("browser_open: {e}");
        return;
    }
    if !url.is_empty() {
        if let Err(e) = crate::browser::navigate(url) {
            tracing::error!("browser_open navigate: {e}");
        }
    }
}

pub fn browser_navigate(_svc: &RealService, url: &str) -> Result<(), String> {
    crate::browser::navigate(url)
}

pub fn browser_click(_svc: &RealService, selector: &str) -> Result<(), String> {
    crate::browser::click(selector)
}

pub fn browser_type(_svc: &RealService, selector: &str, text: &str) -> Result<(), String> {
    crate::browser::type_text(selector, text)
}

pub fn browser_screenshot(_svc: &RealService) -> Option<Vec<u8>> {
    crate::browser::screenshot().ok()
}

pub fn browser_eval(_svc: &RealService, js: &str) -> Result<String, String> {
    crate::browser::evaluate_js(js)
}

pub fn browser_read(_svc: &RealService, selector: &str) -> Result<String, String> {
    crate::browser::get_text(selector)
}

pub fn browser_close(_svc: &RealService) {
    crate::browser::close();
}

pub fn browser_url(_svc: &RealService) -> Option<String> {
    crate::browser::current_url().ok()
}

pub fn browser_title(_svc: &RealService) -> Option<String> {
    crate::browser::page_title().ok()
}
