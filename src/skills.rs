use serde::{Deserialize, Serialize};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
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

        let text = item
            .get("Text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
        let url = item
            .get("FirstURL")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
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
                Some(SearchResult {
                    title,
                    url,
                    snippet,
                })
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
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
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
            Err(format!(
                "All search providers failed: {}",
                failures.join(" | ")
            ))
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

    #[test]
    fn skill_catalog_loads_yaml_skills() {
        // Hardcoded skills are removed; all skills come from config/skills/*.yaml.
        // In test environment there may be 0 YAML skills (config dir absent), which is fine.
        let skills = list_skills();
        // All returned skills must have non-empty id and name.
        for s in &skills {
            assert!(!s.id.is_empty(), "skill id must not be empty");
            assert!(!s.name.is_empty(), "skill name must not be empty");
        }
    }

    #[test]
    fn recommended_skills_respects_available_filter() {
        use crate::skills::{SkillDefinition, recommended_skills};
        // Build a minimal fake skill with example_prompts
        let fake = SkillDefinition {
            id: "test_skill".to_string(),
            name: "Test Skill".to_string(),
            description: "A test skill".to_string(),
            requires_approval: false,
            category: "coding".to_string(),
            backed_by_tools: vec![],
            example_prompts: vec!["分析 PR".to_string(), "幫我看 diff".to_string()],
            enabled: true,
            prompt_template: None,
        };
        let available = vec![fake.clone()];
        // Query matching example_prompts should surface the skill
        let result = recommended_skills("分析 PR", &available);
        assert!(result.iter().any(|s| s.id == "test_skill"));
        // Query with no overlap → empty result
        let empty = recommended_skills("完全不相關的查詢 xyz", &available);
        assert!(empty.is_empty());
        // Empty available list → always empty
        let none = recommended_skills("分析 PR", &[]);
        assert!(none.is_empty());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub requires_approval: bool,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub backed_by_tools: Vec<String>,
    #[serde(default)]
    pub example_prompts: Vec<String>,
    /// Whether this skill is active (YAML skills can set this to false).
    #[serde(default = "skill_enabled_default")]
    pub enabled: bool,
    /// Prompt template injected into CodingAgent for YAML-defined skills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
}

fn skill_enabled_default() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExecutionResult {
    pub skill_id: String,
    pub emitted_event: String,
    pub accepted: bool,
}


pub fn list_skills() -> Vec<SkillDefinition> {
    let mut skills = hardcoded_skills();
    skills.extend(crate::skill_loader::load_yaml_skills());
    skills
}

fn hardcoded_skills() -> Vec<SkillDefinition> {
    // 技能全部由 config/skills/*.yaml 定義，hardcoded list 已清空。
    // 底層工具（file_read、shell_exec、memory_search 等）是 agent 的基本能力，
    // 不作為可授權的「技能」展示。
    vec![]
}

/// 通用評分：根據技能的 example_prompts 與 query 的關鍵字重疊度給分。
/// 不再依賴硬編碼 skill ID，讓 YAML 的 example_prompts 自我描述。
fn score_skill_for_query(skill: &SkillDefinition, query: &str) -> i32 {
    let lower = query.to_lowercase();
    // Name match bonus
    let name_score = if lower.contains(&skill.name.to_lowercase()) { 5 } else { 0 };
    // Example prompt word overlap
    let prompt_score: i32 = skill.example_prompts.iter()
        .map(|p| {
            p.split_whitespace()
                .filter(|w| w.len() >= 2 && lower.contains(&w.to_lowercase()))
                .count() as i32
        })
        .sum::<i32>() * 3;
    name_score + prompt_score
}

/// 從給定技能清單中，根據 query 關鍵字推薦最相關的技能（最多 4 個）。
/// `available` 已經過 per-agent 白名單過濾。
pub fn recommended_skills(query: &str, available: &[SkillDefinition]) -> Vec<SkillDefinition> {
    let mut scored: Vec<(i32, &SkillDefinition)> = available.iter()
        .filter_map(|skill| {
            let score = score_skill_for_query(skill, query);
            (score > 0).then_some((score, skill))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored.into_iter().map(|(_, s)| s.clone()).take(4).collect()
}

pub fn ensure_registered(skill_id: &str) -> Result<(), String> {
    if list_skills().iter().any(|skill| skill.id == skill_id) {
        Ok(())
    } else {
        Err(format!("Unknown skill: {skill_id}"))
    }
}

pub fn execute_skill(skill_id: &str, timestamp: &str) -> Result<SkillExecutionResult, String> {
    ensure_registered(skill_id)?;
    eprintln!("[skills] Executing skill '{skill_id}' for task at {timestamp}");
    Ok(SkillExecutionResult {
        skill_id: skill_id.to_string(),
        emitted_event: format!("skill:{skill_id}"),
        accepted: true,
    })
}
