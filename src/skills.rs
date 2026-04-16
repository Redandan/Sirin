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
            trigger_keywords: vec![],
            enabled: true,
            prompt_template: None,
            script_file: None,
            script_interpreter: None,
            script_timeout_secs: 30,
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

    #[tokio::test]
    async fn browser_test_skill_list_mode() {
        let out = execute_browser_test("list").await.expect("list");
        // config/tests/ has at least wiki_smoke + example_en
        assert!(out.contains("wiki_smoke"), "output: {out}");
    }

    #[tokio::test]
    async fn browser_test_skill_empty_input_lists() {
        let out = execute_browser_test("").await.expect("empty");
        assert!(out.contains("可用測試") || out.contains("available"));
    }

    #[tokio::test]
    async fn browser_test_skill_unknown_id_guides_user() {
        let out = execute_browser_test("nonexistent_test_xyz").await.expect("unknown");
        assert!(out.contains("未找到") || out.contains("not found"));
        assert!(out.contains("nonexistent_test_xyz"));
    }

    #[test]
    fn browser_test_skill_listed_in_skill_manifest() {
        let skills = list_skills();
        let has_bt = skills.iter().any(|s| s.id == "browser-test");
        assert!(has_bt, "browser-test skill must be in list_skills()");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub requires_approval: bool,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub backed_by_tools: Vec<String>,
    #[serde(default)]
    pub example_prompts: Vec<String>,
    /// Keywords that trigger this skill (high-weight exact match in scoring).
    #[serde(default)]
    pub trigger_keywords: Vec<String>,
    /// Whether this skill is active (YAML skills can set this to false).
    #[serde(default = "skill_enabled_default")]
    pub enabled: bool,
    /// Prompt template injected into LLM context when this skill is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
    /// Path to an external script (relative to project root).
    /// e.g. "config/scripts/vip_maintain.py"
    /// Supported extensions: .rhai (embedded, no install needed), .py (python3), .sh (bash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_file: Option<String>,
    /// Override the interpreter for non-.rhai scripts; auto-inferred from extension if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_interpreter: Option<String>,
    /// Maximum seconds the script may run before being killed (default 30).
    #[serde(default = "default_script_timeout")]
    pub script_timeout_secs: u64,
}

fn skill_enabled_default() -> bool { true }
fn default_script_timeout() -> u64 { 30 }

// ── Built-in skill: browser-test ─────────────────────────────────────────────

/// Execute the `browser-test` skill.  Interprets `user_input`:
/// - `"list"` / empty / `"ls"` → returns the list of available tests
/// - An existing test_id → runs that test, returns status summary
/// - Otherwise → returns guidance (how to run a test)
///
/// For ad-hoc URL-based testing the caller should use the `run_test` tool
/// directly (or the `web_navigate` tool) — the skill layer only runs tests
/// already defined as YAML goals.
async fn execute_browser_test(user_input: &str) -> Result<String, String> {
    let trimmed = user_input.trim();

    // List mode
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("list")
        || trimmed.eq_ignore_ascii_case("ls")
        || trimmed == "列出" || trimmed == "所有"
    {
        let tests = crate::test_runner::list_tests();
        if tests.is_empty() {
            return Ok("目前 config/tests/ 沒有任何測試 YAML。請先建立 goal 檔案。".into());
        }
        let mut out = format!("可用測試 ({}):\n\n", tests.len());
        for t in &tests {
            let tags = if t.tags.is_empty() { String::new() }
                else { format!(" [{}]", t.tags.join(",")) };
            out.push_str(&format!("• {} — {}{}\n  url: {}\n", t.id, t.name, tags, t.url));
        }
        out.push_str("\n執行範例: browser-test <test_id>");
        return Ok(out);
    }

    // Try to match as a test_id (first whitespace-separated token)
    let requested_id = trimmed.split_whitespace().next().unwrap_or(trimmed);
    let tests = crate::test_runner::list_tests();
    let matched = tests.iter().find(|t| t.id == requested_id);

    let Some(test) = matched else {
        let available: Vec<String> = tests.iter().map(|t| t.id.clone()).collect();
        return Ok(format!(
            "未找到 test_id '{requested_id}'。\n\n可用測試 id: {}\n\n\
             提示:\n\
             - `browser-test list` 列出所有測試（含 goal）\n\
             - `browser-test <id>` 執行指定測試",
            available.join(", "),
        ));
    };

    // Run the matched test. Use a minimal AgentContext (no tracker needed).
    let tools = crate::adk::tool::default_tool_registry();
    let ctx = crate::adk::context::AgentContext::new("skill:browser-test", tools);

    let started = std::time::Instant::now();
    let result = crate::test_runner::run_test(&ctx, &test.id, false)
        .await
        .map_err(|e| format!("run_test failed: {e}"))?;

    let elapsed_s = started.elapsed().as_secs_f64();
    let status_label = match result.status {
        crate::test_runner::TestStatus::Passed  => "✓ PASSED",
        crate::test_runner::TestStatus::Failed  => "✗ FAILED",
        crate::test_runner::TestStatus::Timeout => "⏱ TIMEOUT",
        crate::test_runner::TestStatus::Error   => "✗ ERROR",
    };

    let mut summary = format!(
        "{status_label} — {name} ({id})\n\
         duration: {elapsed:.1}s  iterations: {iter}  steps: {steps}\n",
        name = test.name,
        id = test.id,
        elapsed = elapsed_s,
        iter = result.iterations,
        steps = result.history.len(),
    );

    if let Some(analysis) = &result.final_analysis {
        summary.push_str(&format!("\nAnalysis:\n{analysis}\n"));
    }
    if let Some(err) = &result.error_message {
        summary.push_str(&format!("\nError: {err}\n"));
    }
    if let Some(path) = &result.screenshot_path {
        summary.push_str(&format!("\nFailure screenshot: {path}\n"));
    }
    if let Some(err) = &result.screenshot_error {
        summary.push_str(&format!("\nScreenshot error: {err}\n"));
    }

    Ok(summary)
}

/// Infer the script interpreter from the file extension.
fn infer_interpreter(path: &str) -> &'static str {
    if path.ends_with(".py")  { "python3" }
    else if path.ends_with(".sh")  { "bash" }
    else if path.ends_with(".js")  { "node" }
    else { "sh" }
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
    // Trigger keywords — high weight, any match is a strong signal
    let trigger_score: i32 = skill.trigger_keywords.iter()
        .filter(|kw| !kw.is_empty() && lower.contains(kw.to_lowercase().as_str()))
        .count() as i32 * 8;
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
    trigger_score + name_score + prompt_score
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

/// Build a skill context block to inject into the LLM system prompt.
///
/// For each recommended skill:
/// - Script-based (`script_file` present): auto-generates a `skill_execute` call instruction.
/// - Tool-based (`backed_by_tools` non-empty + `prompt_template`): uses the template directly.
///
/// Returns `None` when there is nothing actionable to inject.
pub fn build_skill_context(planner_skill_ids: &[String]) -> Option<String> {
    if planner_skill_ids.is_empty() {
        return None;
    }
    let all = list_skills();
    let mut parts: Vec<String> = Vec::new();

    for id in planner_skill_ids {
        let Some(skill) = all.iter().find(|s| &s.id == id) else { continue };

        let instruction = if skill.script_file.is_some() {
            format!(
                "**{}**：呼叫 `skill_execute` 工具，\
                 參數 `skill_id = \"{}\"`, `user_input = <用戶原始請求>`。\
                 腳本輸出即為最終結果，無需二次加工。",
                skill.name, skill.id
            )
        } else if let Some(tmpl) = &skill.prompt_template {
            format!("**{}**：{}", skill.name, tmpl.trim())
        } else {
            continue;
        };
        parts.push(instruction);
    }

    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "## 本次請求的可用能力（優先使用這些）\n\n{}",
        parts.join("\n\n")
    ))
}


/// Execute a skill's external script.
///
/// The script receives a JSON object on stdin:
/// ```json
/// { "skill_id": "…", "user_input": "…", "agent_id": "…" }
/// ```
/// stdout is captured and returned as the result string.
/// stderr is logged. Non-zero exit code is treated as an error.
pub async fn execute_skill(
    skill_id: &str,
    user_input: &str,
    agent_id: Option<&str>,
) -> Result<String, String> {
    // ── Built-in skills (no script_file required) ────────────────────────────
    if skill_id == "config-check" {
        let issues = tokio::task::spawn_blocking(crate::config_check::run_diagnostics)
            .await
            .map_err(|e| format!("spawn_blocking: {e}"))?;
        return Ok(crate::config_check::format_report(&issues));
    }

    if skill_id == "browser-test" {
        return execute_browser_test(user_input).await;
    }

    let skill = list_skills()
        .into_iter()
        .find(|s| s.id == skill_id)
        .ok_or_else(|| format!("Unknown skill: {skill_id}"))?;

    let script_path = skill.script_file.as_deref().ok_or_else(|| {
        format!("Skill '{skill_id}' has no script_file configured")
    })?;

    if !std::path::Path::new(script_path).exists() {
        return Err(format!("Script not found: {script_path}"));
    }

    // ── Rhai scripts run in-process (no external runtime needed) ─────────────
    if script_path.ends_with(".rhai") {
        let path = script_path.to_string();
        let sid = skill_id.to_string();
        let input = user_input.to_string();
        let aid = agent_id.map(str::to_string);

        let result = tokio::task::spawn_blocking(move || {
            crate::rhai_engine::run_rhai_script(&path, &sid, &input, aid.as_deref())
        })
        .await
        .map_err(|e| format!("Thread error: {e}"))??;

        crate::sirin_log!("[skill] '{}' → {} chars", skill_id, result.len());
        return Ok(result);
    }

    // ── External interpreter (python3, bash, etc.) ────────────────────────────
    let interpreter = skill
        .script_interpreter
        .as_deref()
        .unwrap_or_else(|| infer_interpreter(script_path));

    let stdin_payload = serde_json::json!({
        "skill_id": skill_id,
        "user_input": user_input,
        "agent_id": agent_id,
    });
    let stdin_bytes =
        serde_json::to_vec(&stdin_payload).map_err(|e| format!("Serialize error: {e}"))?;

    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut child = Command::new(interpreter)
        .arg(script_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn '{interpreter}': {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&stdin_bytes)
            .await
            .map_err(|e| format!("Failed to write stdin: {e}"))?;
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(skill.script_timeout_secs),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| format!("Script '{skill_id}' timed out after {}s", skill.script_timeout_secs))?
    .map_err(|e| format!("Script execution error: {e}"))?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        crate::sirin_log!("[skill] '{}' stderr: {}", skill_id, stderr.trim());
    }

    if !output.status.success() {
        return Err(format!(
            "Script '{}' exited {:?}: {}",
            skill_id,
            output.status.code(),
            stderr.trim()
        ));
    }

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    crate::sirin_log!("[skill] '{}' → {} chars", skill_id, result.len());
    Ok(result)
}
