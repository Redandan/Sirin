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

pub fn browser_click_point(_svc: &RealService, x: f64, y: f64) -> Result<(), String> {
    crate::browser::click_point(x, y)
}

pub fn browser_hover(_svc: &RealService, selector: &str) -> Result<(), String> {
    crate::browser::hover(selector)
}

pub fn browser_press_key(_svc: &RealService, key: &str) -> Result<(), String> {
    crate::browser::press_key(key)
}

pub fn browser_wait(_svc: &RealService, selector: &str, timeout_ms: u64) -> Result<(), String> {
    crate::browser::wait_for_ms(selector, timeout_ms)
}

pub fn browser_exists(_svc: &RealService, selector: &str) -> bool {
    crate::browser::element_exists(selector).unwrap_or(false)
}

pub fn browser_select(_svc: &RealService, selector: &str, value: &str) -> Result<(), String> {
    crate::browser::select_option(selector, value)
}

pub fn browser_scroll(_svc: &RealService, x: f64, y: f64) -> Result<(), String> {
    crate::browser::scroll_by(x, y)
}

pub fn browser_set_viewport(_svc: &RealService, width: u32, height: u32, mobile: bool) -> Result<(), String> {
    crate::browser::set_viewport(width, height, 1.0, mobile)
}

pub fn browser_console(_svc: &RealService, limit: usize) -> String {
    crate::browser::console_messages(limit).unwrap_or_else(|_| "[]".into())
}

pub fn browser_tab_count(_svc: &RealService) -> usize {
    crate::browser::list_tabs().map(|t| t.len()).unwrap_or(0)
}
