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

    #[test]
    fn recommended_skills_for_code_question_include_local_tools() {
        let skills = recommended_skills("幫我看 src/main.rs");
        let ids: Vec<String> = skills.into_iter().map(|skill| skill.id).collect();

        assert!(ids.contains(&"local_file_read".to_string()));
        assert!(ids.contains(&"codebase_search".to_string()) || ids.contains(&"project_overview".to_string()));
    }

    #[test]
    fn skill_catalog_exposes_grounded_code_capabilities() {
        let skills = list_skills();
        assert!(skills.iter().any(|skill| skill.id == "local_file_read"));
        assert!(skills.iter().any(|skill| skill.id == "project_overview"));
        assert!(skills.iter().any(|skill| skill.id == "memory_search"));
        assert!(skills.iter().any(|skill| skill.id == "grounded_fix"));
        assert!(skills.iter().any(|skill| skill.id == "symbol_trace"));
    }

    #[test]
    fn recommended_skills_for_optimization_question_surface_planning_and_fixing() {
        let skills = recommended_skills("先分析再改，幫我安全優化這段 code 並跑測試");
        let ids: Vec<String> = skills.into_iter().map(|skill| skill.id).collect();

        assert!(ids.contains(&"code_change_planning".to_string()));
        assert!(ids.contains(&"grounded_fix".to_string()));
        assert!(ids.contains(&"test_selector".to_string()));
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExecutionResult {
    pub skill_id: String,
    pub emitted_event: String,
    pub accepted: bool,
}

fn skill(
    id: &str,
    name: &str,
    description: &str,
    category: &str,
    requires_approval: bool,
    backed_by_tools: &[&str],
    example_prompts: &[&str],
) -> SkillDefinition {
    SkillDefinition {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        requires_approval,
        category: category.to_string(),
        backed_by_tools: backed_by_tools.iter().map(|v| v.to_string()).collect(),
        example_prompts: example_prompts.iter().map(|v| v.to_string()).collect(),
    }
}

pub fn list_skills() -> Vec<SkillDefinition> {
    vec![
        skill(
            "project_overview",
            "Project Overview",
            "先查看幾個核心檔案，整理專案架構、主要模組與工作方式。",
            "code-understanding",
            false,
            &["project_overview", "local_file_read"],
            &["這個專案大概是怎麼運作的？", "列出這個 repo 的核心模組"],
        ),
        skill(
            "local_file_read",
            "Local File Read",
            "讀取真實本地檔案內容，回覆檔案用途、片段與重點。",
            "code-understanding",
            false,
            &["local_file_read"],
            &["幫我看 src/main.rs", "解釋 src/ui.rs"],
        ),
        skill(
            "codebase_search",
            "Codebase Search",
            "在本地程式碼索引中找出相關檔案、模組與符號，再交由 agent 組織答案。",
            "code-understanding",
            false,
            &["codebase_search"],
            &["找出 chat flow 在哪裡", "哪個檔案負責 Telegram listener"],
        ),
        skill(
            "memory_search",
            "Memory Recall",
            "查詢近期對話、研究摘要與記憶內容，協助回答承接型問題。",
            "context-retrieval",
            false,
            &["memory_search"],
            &["剛剛提到的那些檔案是做什麼的", "延續上個問題"],
        ),
        skill(
            "code_change_planning",
            "Code Change Planning",
            "在修改前先整理受影響檔案、預期改動步驟、風險與驗證方式。",
            "code-optimization",
            false,
            &["project_overview", "codebase_search", "memory_search"],
            &["先分析再改", "幫我規劃這次重構", "這段要怎麼安全優化"],
        ),
        skill(
            "symbol_trace",
            "Symbol Trace",
            "追蹤函式、struct 或模組的呼叫鏈與影響範圍，避免改一處壞多處。",
            "code-optimization",
            false,
            &["codebase_search", "local_file_read"],
            &["這個 function 在哪裡被呼叫", "改這個會影響哪些地方"],
        ),
        skill(
            "grounded_fix",
            "Grounded Fix",
            "修 bug 或優化前，先查相關檔案與上下文，再根據本地證據做最小修改。",
            "code-optimization",
            false,
            &["codebase_search", "local_file_read", "memory_search"],
            &["幫我找 root cause 再修", "不要亂改，先看上下文"],
        ),
        skill(
            "test_selector",
            "Targeted Test Selection",
            "根據改動範圍挑出應該先跑的測試或檢查命令，提升本地迭代效率。",
            "code-optimization",
            false,
            &[],
            &["改完幫我測一下", "只驗證聊天流程", "先跑相關測試"],
        ),
        skill(
            "architecture_consistency_check",
            "Architecture Consistency Check",
            "確認修改是否仍符合目前的 agent / ADK 分層與整體專案架構。",
            "code-optimization",
            false,
            &["project_overview", "codebase_search", "local_file_read"],
            &["這樣改會不會破壞架構", "檢查這次重構是否一致"],
        ),
        skill(
            "web_search",
            "Resilient Web Search",
            "透過 SearXNG / DuckDuckGo fallback 搜尋外部資訊，不需額外 API key。",
            "external-research",
            false,
            &["web_search"],
            &["查一下某個函式庫用法", "幫我搜尋 Gemma 4 模型資訊"],
        ),
        skill(
            "send_tg_reply",
            "Send Telegram Reply",
            "發出技能事件，交給 Telegram 模組送出回覆；屬於需要明確授權的動作。",
            "external-action",
            true,
            &[],
            &["回覆 Telegram 訊息", "發一則通知"],
        ),
    ]
}

fn score_skill_for_query(skill_id: &str, query: &str) -> i32 {
    let lower = query.to_lowercase();
    match skill_id {
        "project_overview" => {
            if ["專案", "架構", "結構", "module", "模組", "怎麼運作", "overview"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                10
            } else {
                0
            }
        }
        "local_file_read" => {
            if ["src/", ".rs", ".toml", ".md", "檔案", "文件", "file"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                12
            } else {
                0
            }
        }
        "codebase_search" => {
            if ["哪裡", "在哪", "搜尋", "search", "symbol", "函式", "function", "模組", "src/", ".rs", ".toml"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                9
            } else {
                0
            }
        }
        "memory_search" => {
            if ["剛剛", "上面", "這些", "那些", "前面", "延續"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                9
            } else {
                0
            }
        }
        "code_change_planning" => {
            if ["規劃", "计划", "plan", "重構", "重构", "優化", "优化", "先分析再改"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                10
            } else {
                0
            }
        }
        "symbol_trace" => {
            if ["呼叫", "调用", "trace", "影響", "影响", "哪裡被用", "在哪裡被用"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                10
            } else {
                0
            }
        }
        "grounded_fix" => {
            if ["bug", "修", "fix", "root cause", "問題", "优化", "優化"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                11
            } else {
                0
            }
        }
        "test_selector" => {
            if ["測試", "测试", "test", "驗證", "验证", "check"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                9
            } else {
                0
            }
        }
        "architecture_consistency_check" => {
            if ["架構", "架构", "architecture", "一致", "分層", "分层"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                9
            } else {
                0
            }
        }
        "web_search" => {
            if ["google", "搜尋", "search", "網路", "最新", "查一下"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                8
            } else {
                0
            }
        }
        "send_tg_reply" => {
            if ["telegram", "回覆", "通知", "發送", "send"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                6
            } else {
                0
            }
        }
        _ => 0,
    }
}

pub fn recommended_skills(query: &str) -> Vec<SkillDefinition> {
    let mut scored: Vec<(i32, SkillDefinition)> = list_skills()
        .into_iter()
        .filter_map(|skill| {
            let score = score_skill_for_query(&skill.id, query);
            (score > 0).then_some((score, skill))
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored.into_iter().map(|(_, skill)| skill).take(4).collect()
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
