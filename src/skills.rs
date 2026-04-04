use serde::{Deserialize, Serialize};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const DDG_HTML_URL: &str = "https://duckduckgo.com/html/";
const DDG_INSTANT_URL: &str = "https://api.duckduckgo.com/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngItem>,
}

#[derive(Debug, Deserialize)]
struct SearxngItem {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DdgInstantResponse {
    #[serde(rename = "AbstractText", default)]
    abstract_text: String,
    #[serde(rename = "AbstractURL", default)]
    abstract_url: String,
    #[serde(rename = "RelatedTopics", default)]
    related_topics: Vec<serde_json::Value>,
}

fn trim_snippet(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let head: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

fn dedupe_results(mut results: Vec<SearchResult>, limit: usize) -> Vec<SearchResult> {
    let mut seen = std::collections::HashSet::new();
    results.retain(|item| {
        let key = item.url.trim().to_lowercase();
        !key.is_empty() && seen.insert(key)
    });
    results.truncate(limit);
    results
}

fn is_ddg_bot_challenge(status: reqwest::StatusCode, html: &str) -> bool {
    status == reqwest::StatusCode::ACCEPTED
        || html.contains("anomaly-modal")
        || html.contains("bots use DuckDuckGo too")
        || html.contains("challenge-form")
        || html.contains("Please complete the following challenge")
}

fn collect_instant_topics(values: &[serde_json::Value], out: &mut Vec<SearchResult>) {
    for item in values {
        if let Some(topics) = item.get("Topics").and_then(|v| v.as_array()) {
            collect_instant_topics(topics, out);
            continue;
        }

        let text = item.get("Text").and_then(|v| v.as_str()).unwrap_or_default().trim();
        let url = item.get("FirstURL").and_then(|v| v.as_str()).unwrap_or_default().trim();
        if !text.is_empty() && !url.is_empty() {
            out.push(SearchResult {
                title: trim_snippet(text, 80),
                url: url.to_string(),
                snippet: trim_snippet(text, 160),
            });
        }
    }
}

async fn search_searxng(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let base = std::env::var("SEARXNG_BASE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "SEARXNG_BASE_URL not configured".to_string())?;

    let url = format!("{base}/search");
    let resp: SearxngResponse = client
        .get(url)
        .query(&[("q", query), ("format", "json"), ("categories", "general")])
        .send()
        .await
        .map_err(|e| format!("SearXNG request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("SearXNG returned error status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse SearXNG JSON: {e}"))?;

    let results = resp
        .results
        .into_iter()
        .filter_map(|item| {
            let title = item.title.unwrap_or_default().trim().to_string();
            let url = item.url.unwrap_or_default().trim().to_string();
            let snippet = trim_snippet(item.content.as_deref().unwrap_or_default(), 180);
            if title.is_empty() || url.is_empty() {
                None
            } else {
                Some(SearchResult { title, url, snippet })
            }
        })
        .collect::<Vec<_>>();

    let deduped = dedupe_results(results, 10);
    if deduped.is_empty() {
        Err("SearXNG returned no usable results".to_string())
    } else {
        Ok(deduped)
    }
}

async fn search_ddg_instant(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let resp: DdgInstantResponse = client
        .get(DDG_INSTANT_URL)
        .query(&[
            ("q", query),
            ("format", "json"),
            ("no_html", "1"),
            ("no_redirect", "1"),
            ("skip_disambig", "0"),
        ])
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo instant request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("DuckDuckGo instant status error: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse DuckDuckGo instant JSON: {e}"))?;

    let mut results = Vec::new();
    if !resp.abstract_text.trim().is_empty() && !resp.abstract_url.trim().is_empty() {
        results.push(SearchResult {
            title: trim_snippet(&resp.abstract_text, 80),
            url: resp.abstract_url,
            snippet: trim_snippet(&resp.abstract_text, 180),
        });
    }
    collect_instant_topics(&resp.related_topics, &mut results);

    let deduped = dedupe_results(results, 10);
    if deduped.is_empty() {
        Err("DuckDuckGo instant API returned no related topics".to_string())
    } else {
        Ok(deduped)
    }
}

async fn search_ddg_html(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let resp = client
        .get(DDG_HTML_URL)
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo HTML request failed: {e}"))?;

    let status = resp.status();
    let html = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read DuckDuckGo HTML body: {e}"))?;

    if is_ddg_bot_challenge(status, &html) {
        return Err(format!(
            "DuckDuckGo blocked the automated request with a bot challenge (HTTP {})",
            status.as_u16()
        ));
    }

    let document = scraper::Html::parse_document(&html);
    let result_sel = scraper::Selector::parse(".result__body")
        .map_err(|_| "Failed to parse DuckDuckGo result selector")?;
    let title_sel = scraper::Selector::parse(".result__title a")
        .map_err(|_| "Failed to parse DuckDuckGo title selector")?;
    let snippet_sel = scraper::Selector::parse(".result__snippet")
        .map_err(|_| "Failed to parse DuckDuckGo snippet selector")?;

    let mut results = Vec::new();
    for card in document.select(&result_sel).take(10) {
        let title_el = card.select(&title_sel).next();
        let snippet_el = card.select(&snippet_sel).next();

        let title = title_el
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let url = title_el
            .and_then(|el| el.value().attr("href"))
            .unwrap_or_default()
            .to_string();

        let snippet = snippet_el
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult { title, url, snippet });
        }
    }

    let deduped = dedupe_results(results, 10);
    if deduped.is_empty() {
        Err("DuckDuckGo HTML returned no organic results".to_string())
    } else {
        Ok(deduped)
    }
}

/// Perform a zero-key web search with graceful provider fallback.
///
/// Order: `SearXNG (optional)` → `DuckDuckGo instant answers` → `DuckDuckGo HTML`.
/// The function returns a descriptive error instead of silently treating bot
/// challenges as empty search results.
pub async fn ddg_search(query: &str) -> Result<Vec<SearchResult>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let mut failures = Vec::new();

    match search_searxng(&client, query).await {
        Ok(results) => return Ok(results),
        Err(e) => failures.push(e),
    }

    match search_ddg_instant(&client, query).await {
        Ok(results) => return Ok(results),
        Err(e) => failures.push(e),
    }

    match search_ddg_html(&client, query).await {
        Ok(results) => Ok(results),
        Err(e) => {
            failures.push(e);
            Err(format!("All search providers failed: {}", failures.join(" | ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ddg_challenge_pages() {
        let html = "<div class='anomaly-modal'>Please complete the following challenge</div>";
        assert!(is_ddg_bot_challenge(reqwest::StatusCode::ACCEPTED, html));
    }

    #[test]
    fn deduplicates_search_results() {
        let results = dedupe_results(
            vec![
                SearchResult {
                    title: "One".into(),
                    url: "https://example.com".into(),
                    snippet: "A".into(),
                },
                SearchResult {
                    title: "Duplicate".into(),
                    url: "https://example.com".into(),
                    snippet: "B".into(),
                },
            ],
            10,
        );
        assert_eq!(results.len(), 1);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub requires_approval: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExecutionResult {
    pub skill_id: String,
    pub emitted_event: String,
    pub accepted: bool,
}

pub fn list_skills() -> Vec<SkillDefinition> {
    vec![
        SkillDefinition {
            id: "send_tg_reply".to_string(),
            name: "Send Telegram Reply".to_string(),
            description: "Emits a skill event for the Telegram module to send a reply.".to_string(),
            requires_approval: true,
        },
        SkillDefinition {
            id: "web_search".to_string(),
            name: "Resilient Web Search".to_string(),
            description: "Searches the web via fallback providers without requiring an API key by default.".to_string(),
            requires_approval: false,
        },
    ]
}

pub fn ensure_registered(skill_id: &str) -> Result<(), String> {
    if list_skills().iter().any(|skill| skill.id == skill_id) {
        Ok(())
    } else {
        Err(format!("Unknown skill: {skill_id}"))
    }
}

pub fn execute_skill(
    skill_id: &str,
    timestamp: &str,
) -> Result<SkillExecutionResult, String> {
    ensure_registered(skill_id)?;
    eprintln!("[skills] Executing skill '{skill_id}' for task at {timestamp}");
    Ok(SkillExecutionResult {
        skill_id: skill_id.to_string(),
        emitted_event: format!("skill:{skill_id}"),
        accepted: true,
    })
}
