//! Five-phase research pipeline.
//!
//! Phase 1: fetch page (if URL given).
//! Phase 2: LLM overview of the content / topic.
//! Phase 3: LLM generates 4 follow-up research questions.
//! Phase 4: parallel DDG search + LLM answer per question.
//! Phase 5: LLM synthesises the final report.  The final report is persisted
//!          to the FTS5 memory store as a `research` entry.
//!
//! Every 5th successful research task triggers `maybe_reflect_on_objectives`
//! which asks the LLM whether the persona's objectives should be updated;
//! the proposed objectives go to the UI review slot rather than being
//! written directly.

use crate::events;
use crate::llm::{call_prompt, LlmConfig};
use crate::memory::memory_store;
use crate::persona::Persona;
use crate::sirin_log;
use crate::skills::ddg_search;

use super::fetch::fetch_page_text;
use super::persistence::save_research;
use super::{store_pending_objectives, ResearchStep, ResearchTask};

/// Max chars fed to LLM per context block.
const MAX_CONTEXT: usize = 2000;

pub(super) async fn pipeline(
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
        "",
        "shared",
    ) {
        sirin_log!("[researcher] Failed to persist research memory: {e}");
    }

    task.final_report = Some(report);
    let _ = save_research(task);

    Ok(())
}

/// After every 5th completed research, ask the LLM whether the persona's
/// objectives should be updated, and store the proposal in the UI review slot.
pub(super) async fn maybe_reflect_on_objectives(
    http: &reqwest::Client,
    llm: &LlmConfig,
    task: &ResearchTask,
) {
    let persona = match Persona::cached() {
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
