use serde::{Deserialize, Serialize};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Perform a zero-key web search via DuckDuckGo HTML endpoint.
///
/// Returns up to 10 organic results scraped from the HTML response.
pub async fn ddg_search(query: &str) -> Result<Vec<SearchResult>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let html = client
        .get("https://duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let document = scraper::Html::parse_document(&html);

    // DuckDuckGo HTML result cards use class "result__body"
    let result_sel = scraper::Selector::parse(".result__body")
        .map_err(|_| "Failed to parse result selector")?;
    let title_sel = scraper::Selector::parse(".result__title a")
        .map_err(|_| "Failed to parse title selector")?;
    let snippet_sel = scraper::Selector::parse(".result__snippet")
        .map_err(|_| "Failed to parse snippet selector")?;

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

    Ok(results)
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
            name: "Zero-Key Web Search".to_string(),
            description: "Searches the web via DuckDuckGo without requiring an API key.".to_string(),
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
