//! HTTP fetching + HTML text extraction for the research pipeline.
//!
//! `scraping_http` returns a process-wide Reqwest client tuned for scraping
//! (custom User-Agent, 60 s timeout).  `fetch_page_text` pulls a URL and
//! concatenates readable body text with deduplication, capped at
//! `MAX_PAGE_TEXT` characters.

use regex::Regex;
use std::sync::OnceLock;

pub(super) const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Max chars extracted from a fetched webpage.
const MAX_PAGE_TEXT: usize = 4000;

/// Scraping-optimized HTTP client: custom User-Agent + 60 s timeout.
pub(super) fn scraping_http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build researcher HTTP client")
    })
}

/// Strip `<script>`, `<style>`, `<noscript>` and `<head>` blocks, then all
/// remaining HTML tags.  Pure-Rust regex — no C parser dependency.
fn extract_text_from_html(html: &str) -> String {
    static STRIP_ELEM: OnceLock<Regex> = OnceLock::new();
    static STRIP_TAGS: OnceLock<Regex> = OnceLock::new();

    let strip_elem = STRIP_ELEM.get_or_init(|| {
        Regex::new(r"(?si)<(script|style|noscript|head)[^>]*>.*?</\1>").unwrap()
    });
    let strip_tags = STRIP_TAGS.get_or_init(|| {
        Regex::new(r"<[^>]+>").unwrap()
    });

    let cleaned = strip_elem.replace_all(html, " ");
    let plain   = strip_tags.replace_all(&cleaned, " ");

    // Basic HTML entity decoding.
    let plain = plain
        .replace("&amp;",  "&")
        .replace("&lt;",   "<")
        .replace("&gt;",   ">")
        .replace("&nbsp;", " ")
        .replace("&#39;",  "'")
        .replace("&quot;", "\"");

    let mut seen = std::collections::HashSet::new();
    plain
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| l.len() > 20 && seen.insert(l.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fetch a URL and extract readable text from the HTML body.
pub(super) async fn fetch_page_text(http: &reqwest::Client, url: &str) -> Result<String, String> {
    let html = http
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("Fetch failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Read body failed: {e}"))?;

    let text = extract_text_from_html(&html);
    Ok(text.chars().take(MAX_PAGE_TEXT).collect())
}
