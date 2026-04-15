//! HTTP fetching + HTML text extraction for the research pipeline.
//!
//! `scraping_http` returns a process-wide Reqwest client tuned for scraping
//! (custom User-Agent, 60 s timeout).  `fetch_page_text` pulls a URL and
//! concatenates readable body text with deduplication, capped at
//! `MAX_PAGE_TEXT` characters.

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

/// Returns a cached `Selector` for page content extraction (compiled once per process).
fn page_content_selector() -> &'static scraper::Selector {
    static SEL: OnceLock<scraper::Selector> = OnceLock::new();
    SEL.get_or_init(|| {
        scraper::Selector::parse("body p, body h1, body h2, body h3, body li, body span, body div")
            .unwrap()
    })
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

    let doc = scraper::Html::parse_document(&html);

    // Remove script / style elements from consideration by only selecting body text nodes.
    let sel = page_content_selector();

    let mut parts: Vec<String> = Vec::new();
    for el in doc.select(&sel) {
        let text: String = el.text().collect::<Vec<_>>().join(" ");
        let trimmed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if trimmed.len() > 20 {
            parts.push(trimmed);
        }
    }

    let combined = parts.join("\n");
    // Deduplicate adjacent identical lines and truncate.
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<&str> = combined
        .lines()
        .filter(|l| seen.insert(l.to_string()))
        .collect();

    let result = deduped.join("\n");
    Ok(result.chars().take(MAX_PAGE_TEXT).collect())
}
