//! Background research agent.
//!
//! Runs a multi-step LLM research pipeline on a URL or topic.
//! Since the LLM is local, the pipeline calls it many times to:
//!   1. Fetch & extract page content (if URL given)
//!   2. Produce an overview analysis
//!   3. Generate follow-up research questions
//!   4. Search + analyse each question (one LLM call per question)
//!   5. Synthesize into a final report
//!
//! All intermediate steps are persisted to `research.jsonl` so the
//! frontend and follow-up worker can track progress.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// ── Persona safety gate ────────────────────────────────────────────────────────

/// Proposed objective update waiting for user confirmation in the UI.
/// `maybe_reflect_on_objectives` stores here instead of writing directly.
fn pending_objectives_slot() -> &'static Mutex<Option<Vec<String>>> {
    static SLOT: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Take the pending objectives out of the slot (returns `None` if nothing pending).
/// Called by the UI on each refresh cycle.
pub fn take_pending_objectives() -> Option<Vec<String>> {
    pending_objectives_slot().lock().ok()?.take()
}

fn store_pending_objectives(objectives: Vec<String>) {
    if let Ok(mut guard) = pending_objectives_slot().lock() {
        *guard = Some(objectives);
    }
}

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::events;
use crate::llm::{call_prompt, LlmConfig};
use crate::memory::memory_store;
use crate::persona::Persona;
use crate::sirin_log;
use crate::skills::ddg_search;

// ── constants ─────────────────────────────────────────────────────────────────

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Max chars extracted from a fetched webpage.
const MAX_PAGE_TEXT: usize = 4000;
/// Max chars fed to LLM per context block.
const MAX_CONTEXT: usize = 2000;

// ── Page fetching ─────────────────────────────────────────────────────────────

/// Returns a cached `Selector` for page content extraction (compiled once per process).
fn page_content_selector() -> &'static scraper::Selector {
    static SEL: OnceLock<scraper::Selector> = OnceLock::new();
    SEL.get_or_init(|| {
        scraper::Selector::parse("body p, body h1, body h2, body h3, body li, body span, body div")
            .unwrap()
    })
}

/// Fetch a URL and extract readable text from the HTML body.
async fn fetch_page_text(http: &reqwest::Client, url: &str) -> Result<String, String> {
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

// ── Research task types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResearchStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchStep {
    pub phase: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchTask {
    pub id: String,
    pub topic: String,
    pub url: Option<String>,
    pub status: ResearchStatus,
    pub steps: Vec<ResearchStep>,
    pub final_report: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

// ── Persistence ───────────────────────────────────────────────────────────────

fn research_log_path() -> PathBuf {
    crate::platform::app_data_dir().join("tracking").join("research.jsonl")
}

fn research_store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn save_research(task: &ResearchTask) -> Result<(), String> {
    let _guard = research_store_lock()
        .lock()
        .map_err(|_| "research store lock poisoned".to_string())?;

    let path = research_log_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // Load all lines, replace matching id, rewrite.
    let existing: Vec<String> = if path.exists() {
        let file = fs::File::open(&path).map_err(|e| e.to_string())?;
        BufReader::new(file)
            .lines()
            .filter_map(|l| l.ok())
            .filter(|l| !l.trim().is_empty())
            .collect()
    } else {
        Vec::new()
    };

    let new_line = serde_json::to_string(task).map_err(|e| e.to_string())?;
    let mut found = false;
    let mut updated: Vec<String> = existing
        .into_iter()
        .map(|line| {
            if let Ok(t) = serde_json::from_str::<ResearchTask>(&line) {
                if t.id == task.id {
                    found = true;
                    return new_line.clone();
                }
            }
            line
        })
        .collect();

    if !found {
        updated.push(new_line);
    }

    let tmp = path.with_extension("jsonl.tmp");
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    for line in &updated {
        writeln!(f, "{line}").map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn list_research() -> Result<Vec<ResearchTask>, String> {
    let _guard = research_store_lock()
        .lock()
        .map_err(|_| "research store lock poisoned".to_string())?;

    let path = research_log_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(&path).map_err(|e| e.to_string())?;
    Ok(BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ResearchTask>(&l).ok())
        .collect())
}

pub fn get_research(id: &str) -> Result<Option<ResearchTask>, String> {
    Ok(list_research()?.into_iter().find(|t| t.id == id))
}

/// Truncate the research log file to empty, removing all records.
pub fn clear_research() -> Result<(), String> {
    let _guard = research_store_lock()
        .lock()
        .map_err(|_| "research store lock poisoned".to_string())?;
    let path = research_log_path();
    if path.exists() {
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

/// Scraping-optimized HTTP client: custom User-Agent + 60 s timeout.
fn scraping_http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build researcher HTTP client")
    })
}

/// Run the full research pipeline and return the completed task.
///
/// This is designed to be spawned as a background tokio task.
pub async fn run_research(topic: String, url: Option<String>) -> ResearchTask {
    let id = format!("r-{}", Utc::now().timestamp_millis());
    let mut task = ResearchTask {
        id: id.clone(),
        topic: topic.clone(),
        url: url.clone(),
        status: ResearchStatus::Running,
        steps: Vec::new(),
        final_report: None,
        started_at: Utc::now().to_rfc3339(),
        finished_at: None,
    };

    let _ = save_research(&task);

    let scrape_http = scraping_http();
    let llm_http = crate::llm::shared_http();
    let llm_arc = crate::llm::shared_llm();
    let llm = llm_arc.as_ref();

    // Run the pipeline; on any hard failure record it and return.
    match pipeline(scrape_http, &llm_http, llm, &mut task).await {
        Ok(_) => {
            task.status = ResearchStatus::Done;
            task.finished_at = Some(Utc::now().to_rfc3339());
        }
        Err(e) => {
            sirin_log!("[researcher] Pipeline failed for '{}': {e}", task.topic);
            task.steps.push(ResearchStep {
                phase: "error".into(),
                output: e.clone(),
            });
            task.status = ResearchStatus::Failed;
            task.final_report = Some(format!("調研失敗：{e}"));
            task.finished_at = Some(Utc::now().to_rfc3339());
        }
    }

    let _ = save_research(&task);

    // Publish completion event so other agents react immediately.
    events::publish(events::AgentEvent::ResearchCompleted {
        topic: task.topic.clone(),
        task_id: task.id.clone(),
        success: task.status == ResearchStatus::Done,
    });

    // Every 5th successful research task, reflect on persona objectives.
    if task.status == ResearchStatus::Done {
        let done_count = list_research()
            .unwrap_or_default()
            .iter()
            .filter(|t| t.status == ResearchStatus::Done)
            .count();
        if done_count % 5 == 0 {
            maybe_reflect_on_objectives(
                crate::llm::shared_http().as_ref(),
                &crate::llm::shared_router_llm(),
                &task,
            )
            .await;
        }
    }

    task
}

/// After every 5th completed research, ask the LLM whether the persona's
/// objectives should be updated, and write the result back to persona.yaml.
async fn maybe_reflect_on_objectives(http: &reqwest::Client, llm: &LlmConfig, task: &ResearchTask) {
    let persona = match Persona::load() {
        Ok(p) => p,
        Err(e) => {
            sirin_log!("[researcher] Persona load failed during reflection: {e}");
            return;
        }
    };

    let report_snippet: String = task
        .final_report
        .as_deref()
        .unwrap_or("")
        .chars()
        .take(800)
        .collect();

    let prompt = format!(
        r#"You are reviewing an AI agent's objectives after completing a research task.

Current objectives:
{objectives}

Latest research topic: {topic}
Research summary:
{report}

Should any objective be added, removed, or refined based on this research?
Reply with a JSON array of updated objectives (same language as original).
Keep it to 2-5 concise objectives. If no change is needed, return the original list.

Output ONLY the JSON array, e.g.: ["Objective 1", "Objective 2"]"#,
        objectives = persona.objectives.join("\n- "),
        topic = task.topic,
        report = report_snippet,
    );

    let raw = match call_prompt(http, llm, prompt).await {
        Ok(r) => r,
        Err(e) => {
            sirin_log!("[researcher] Reflection LLM call failed: {e}");
            return;
        }
    };

    // Extract JSON array from response.
    let start = match raw.find('[') {
        Some(i) => i,
        None => return,
    };
    let end = match raw.rfind(']') {
        Some(i) => i + 1,
        None => return,
    };

    let new_objectives: Vec<String> = match serde_json::from_str(&raw[start..end]) {
        Ok(v) => v,
        Err(e) => {
            sirin_log!("[researcher] Failed to parse reflection JSON: {e}");
            return;
        }
    };

    if new_objectives.is_empty() || new_objectives == persona.objectives {
        return;
    }

    // Store for UI review instead of writing directly.
    sirin_log!(
        "[researcher] Proposed objective update ready for review: {:?}",
        new_objectives
    );
    store_pending_objectives(new_objectives.clone());
    events::publish(events::AgentEvent::PersonaUpdated { new_objectives });
}

async fn pipeline(
    scrape_http: &reqwest::Client,
    llm_http: &reqwest::Client,
    llm: &LlmConfig,
    task: &mut ResearchTask,
) -> Result<(), String> {
    // ── Phase 1: Fetch page (if URL given) ────────────────────────────────────
    let page_text: Option<String> = if let Some(ref url) = task.url {
        sirin_log!("[researcher] Phase 1: fetching {url}");
        match fetch_page_text(scrape_http, url).await {
            Ok(text) => {
                sirin_log!("[researcher] Fetched {} chars", text.len());
                task.steps.push(ResearchStep {
                    phase: "fetch".into(),
                    output: format!("已擷取 {} 字元內容", text.len()),
                });
                let _ = save_research(task);
                Some(text)
            }
            Err(e) => {
                sirin_log!("[researcher] Fetch failed: {e}");
                task.steps.push(ResearchStep {
                    phase: "fetch".into(),
                    output: format!("頁面擷取失敗（{e}），改以 topic 調研"),
                });
                let _ = save_research(task);
                None
            }
        }
    } else {
        None
    };

    // ── Phase 2: Overview analysis ────────────────────────────────────────────
    sirin_log!("[researcher] Phase 2: overview analysis");
    let context_for_overview = match &page_text {
        Some(text) => {
            let snippet: String = text.chars().take(MAX_CONTEXT).collect();
            format!(
                "URL: {}\n\nPage content:\n{snippet}",
                task.url.as_deref().unwrap_or("")
            )
        }
        None => format!("Research topic: {}", task.topic),
    };

    let overview_prompt = format!(
        "You are an expert analyst. Analyze the following and provide a structured overview.\n\
         Respond in Traditional Chinese.\n\
         Format your response as:\n\
         【是什麼】2-3 sentences about what it is\n\
         【主要功能】bullet list of main features/purpose\n\
         【關鍵技術/實體】important names, technologies, or entities mentioned\n\
         \n\
         Input:\n{context_for_overview}\n\
         \n\
         Provide your structured overview:"
    );

    let overview = call_prompt(llm_http, llm, &overview_prompt)
        .await
        .map_err(|e| e.to_string())?;
    sirin_log!("[researcher] Overview done ({} chars)", overview.len());
    task.steps.push(ResearchStep {
        phase: "overview".into(),
        output: overview.clone(),
    });
    let _ = save_research(task);

    // ── Phase 3: Generate research questions ──────────────────────────────────
    sirin_log!("[researcher] Phase 3: generating research questions");
    let questions_prompt = format!(
        "Based on this overview, generate exactly 4 specific research questions \
         to investigate further. These questions should uncover deeper insights.\n\
         Respond in Traditional Chinese.\n\
         Output format: one question per line, numbered 1-4. No extra text.\n\
         \n\
         Overview:\n{overview}\n\
         \n\
         4 research questions:"
    );

    let questions_raw = call_prompt(llm_http, llm, &questions_prompt)
        .await
        .map_err(|e| e.to_string())?;
    let questions: Vec<String> = questions_raw
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            // Strip leading "1. " "2. " etc.
            let q = trimmed
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')', ' '])
                .trim()
                .to_string();
            if q.len() > 5 {
                Some(q)
            } else {
                None
            }
        })
        .take(4)
        .collect();

    sirin_log!("[researcher] Generated {} questions", questions.len());
    task.steps.push(ResearchStep {
        phase: "questions".into(),
        output: questions.join("\n"),
    });
    let _ = save_research(task);

    // ── Phase 4: Search + analyse each question (parallel) ───────────────────
    sirin_log!(
        "[researcher] Phase 4: running {} questions in parallel",
        questions.len()
    );

    let qa_futures: Vec<_> = questions
        .iter()
        .enumerate()
        .map(|(i, question)| {
            let llm_http = llm_http.clone();
            let llm = llm.clone();
            let question = question.clone();
            async move {
                sirin_log!("[researcher] Phase 4.{}: searching for '{}'", i + 1, question);
                let search_results = ddg_search(&question).await.unwrap_or_default();
                let search_block = if search_results.is_empty() {
                    "（無搜尋結果）".to_string()
                } else {
                    search_results
                        .iter()
                        .take(3)
                        .map(|r| format!("- {}: {} ({})", r.title, r.snippet, r.url))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let qa_prompt = format!(
                    "Research question: {question}\n\
                     \n\
                     Web search results:\n{search_block}\n\
                     \n\
                     Based on the search results, provide a concise answer to the research question.\n\
                     Respond in Traditional Chinese. 3-5 sentences max.\n\
                     \n\
                     Answer:"
                );
                let answer = call_prompt(&llm_http, &llm, &qa_prompt)
                    .await
                    .map_err(|e: Box<dyn std::error::Error + Send + Sync>| e.to_string())?;
                sirin_log!("[researcher] Q{} answered ({} chars)", i + 1, answer.len());
                Ok::<(usize, String, String), String>((i, question, answer))
            }
        })
        .collect();

    let qa_outcomes = futures::future::join_all(qa_futures).await;

    // Collect results in original order; propagate first hard error if all fail.
    let mut qa_results: Vec<String> = Vec::new();
    let mut any_success = false;
    for outcome in qa_outcomes {
        match outcome {
            Ok((i, question, answer)) => {
                any_success = true;
                let qa_summary = format!("Q: {question}\nA: {answer}");
                qa_results.push(qa_summary.clone());
                task.steps.push(ResearchStep {
                    phase: format!("research_q{}", i + 1),
                    output: qa_summary,
                });
            }
            Err(e) => {
                sirin_log!("[researcher] A question failed (skipping): {e}");
            }
        }
    }
    if !any_success && !questions.is_empty() {
        return Err("All parallel research questions failed".to_string());
    }
    let _ = save_research(task);

    // ── Phase 5: Synthesise final report ──────────────────────────────────────
    sirin_log!("[researcher] Phase 5: synthesising final report");
    let all_qa = qa_results.join("\n\n---\n\n");
    let overview_snippet: String = overview.chars().take(800).collect();

    let synthesis_prompt = format!(
        "You are a senior analyst writing a research report.\n\
         Respond in Traditional Chinese.\n\
         \n\
         Topic: {topic}\n\
         URL: {url}\n\
         \n\
         Overview analysis:\n{overview_snippet}\n\
         \n\
         Research findings (Q&A):\n{all_qa}\n\
         \n\
         Write a comprehensive research report with these sections:\n\
         【執行摘要】3 sentences\n\
         【核心發現】bullet points of key findings\n\
         【詳細分析】deeper analysis\n\
         【結論與建議】conclusions and recommendations\n\
         \n\
         Research report:",
        topic = task.topic,
        url = task.url.as_deref().unwrap_or("N/A"),
    );

    let report = call_prompt(llm_http, llm, &synthesis_prompt)
        .await
        .map_err(|e| e.to_string())?;
    sirin_log!(
        "[researcher] Final report generated ({} chars)",
        report.len()
    );

    task.steps.push(ResearchStep {
        phase: "synthesis".into(),
        output: format!("報告已生成 ({} chars)", report.len()),
    });

    let memory_snippet: String = report.chars().take(2000).collect();
    if let Err(e) = memory_store(
        &format!("Research topic: {}\n\n{}", task.topic, memory_snippet),
        "research",
    ) {
        sirin_log!("[researcher] Failed to persist research memory: {e}");
    }

    task.final_report = Some(report);
    let _ = save_research(task);

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str, status: ResearchStatus) -> ResearchTask {
        ResearchTask {
            id: id.to_string(),
            topic: format!("test topic {id}"),
            url: None,
            status,
            steps: vec![ResearchStep {
                phase: "overview".into(),
                output: "Test output".into(),
            }],
            final_report: Some("Test report".into()),
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: Some(chrono::Utc::now().to_rfc3339()),
        }
    }

    #[test]
    fn persistence_save_and_get() {
        let id = format!("unit-{}", chrono::Utc::now().timestamp_millis());
        let task = make_task(&id, ResearchStatus::Done);
        save_research(&task).expect("save failed");

        let found = get_research(&id).expect("get failed").expect("not found");
        assert_eq!(found.id, id);
        assert_eq!(found.final_report.as_deref(), Some("Test report"));

        println!("✅ save → get roundtrip OK (id={id})");
    }

    #[test]
    fn persistence_update_overwrites() {
        let id = format!("upd-{}", chrono::Utc::now().timestamp_millis());

        let mut task = make_task(&id, ResearchStatus::Running);
        task.final_report = None;
        save_research(&task).expect("initial save failed");

        task.status = ResearchStatus::Done;
        task.final_report = Some("Updated".into());
        save_research(&task).expect("update failed");

        let all = list_research().expect("list failed");
        let matches: Vec<_> = all.iter().filter(|t| t.id == id).collect();
        assert_eq!(matches.len(), 1, "expected 1 entry, got {}", matches.len());
        assert_eq!(matches[0].status, ResearchStatus::Done);
        assert_eq!(matches[0].final_report.as_deref(), Some("Updated"));

        println!("✅ update/overwrite OK (id={id})");
    }

    #[test]
    fn persistence_list_contains_saved() {
        let id = format!("lst-{}", chrono::Utc::now().timestamp_millis());
        let task = make_task(&id, ResearchStatus::Done);
        save_research(&task).expect("save failed");

        let list = list_research().expect("list failed");
        assert!(
            list.iter().any(|t| t.id == id),
            "saved task not found in list"
        );
        println!("✅ list contains saved task (id={id})");
    }

    /// Full pipeline — requires LM Studio at localhost:1234.
    /// Run with: cargo test pipeline_full -- --nocapture --ignored
    #[tokio::test]
    #[ignore]
    async fn pipeline_full_topic_only() {
        println!("\n======================================");
        println!("🔬 researcher::run_research (topic only)");
        println!("======================================");

        let task = run_research("Rust async/await 底層工作原理".to_string(), None).await;

        println!("  id     = {}", task.id);
        println!("  status = {:?}", task.status);
        println!("  steps  = {}", task.steps.len());
        for s in &task.steps {
            println!("    [{}] {} chars", s.phase, s.output.len());
        }

        assert_ne!(
            task.status,
            ResearchStatus::Failed,
            "pipeline failed: {}",
            task.final_report.as_deref().unwrap_or("")
        );
        assert!(task.final_report.is_some());

        if let Some(report) = &task.final_report {
            println!("\n--- report (first 400 chars) ---");
            println!("{}", &report.chars().take(400).collect::<String>());
        }
        println!("\n✅ pipeline_full_topic_only passed");
    }

    /// Full pipeline with URL — requires LM Studio at localhost:1234.
    /// Run with: cargo test pipeline_url -- --nocapture --ignored
    #[tokio::test]
    #[ignore]
    async fn pipeline_full_with_url() {
        println!("\n======================================");
        println!("🔬 researcher::run_research (URL)");
        println!("======================================");

        let task = run_research(
            "AgoraMarket 平台功能分析".to_string(),
            Some("https://agoramarket.purrtechllc.com/".to_string()),
        )
        .await;

        println!("  id     = {}", task.id);
        println!("  status = {:?}", task.status);
        println!("  steps  = {}", task.steps.len());
        for s in &task.steps {
            println!("    [{}] {} chars", s.phase, s.output.len());
        }

        let has_fetch = task.steps.iter().any(|s| s.phase == "fetch");
        assert!(has_fetch, "fetch phase missing — URL was provided");
        assert_ne!(
            task.status,
            ResearchStatus::Failed,
            "pipeline failed: {}",
            task.final_report.as_deref().unwrap_or("")
        );

        if let Some(report) = &task.final_report {
            println!("\n--- report (first 600 chars) ---");
            println!("{}", &report.chars().take(600).collect::<String>());
        }
        println!("\n✅ pipeline_full_with_url passed");
    }
}
