/// Integration test: full research pipeline
///
/// Simulates the same flow as researcher::run_research():
///   fetch URL → overview LLM → generate questions → DDG search × 4 → synthesis
///
/// Requires LM Studio running at http://localhost:1234
///
/// Run with:
///   cargo test research -- --nocapture

use serde::{Deserialize, Serialize};

const LM_STUDIO_URL: &str = "http://localhost:1234/v1/chat/completions";
const DEFAULT_LM_STUDIO_MODEL: &str = "gemma-4-e4b-it";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

fn lm_studio_model() -> String {
    std::env::var("LM_STUDIO_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_LM_STUDIO_MODEL.to_string())
}

const MAX_PAGE_TEXT: usize = 3000;

// ── LM Studio types ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatReq<'a> {
    model: &'a str,
    messages: Vec<ChatMsg>,
    stream: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct ChatMsg {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMsg,
}

async fn llm(prompt: &str) -> String {
    let client = reqwest::Client::new();
    let model = lm_studio_model();
    let body = ChatReq {
        model: &model,
        messages: vec![ChatMsg { role: "user".into(), content: prompt.into() }],
        stream: false,
    };
    let resp: ChatResp = client
        .post(LM_STUDIO_URL)
        .json(&body)
        .send()
        .await
        .expect("LM Studio request failed")
        .json()
        .await
        .expect("LM Studio JSON parse failed");

    resp.choices
        .first()
        .map(|c| c.message.content.trim().to_string())
        .unwrap_or_default()
}

// ── DuckDuckGo search ─────────────────────────────────────────────────────────

#[derive(Debug)]
struct SearchResult {
    title: String,
    snippet: String,
    url: String,
}

async fn ddg_search(query: &str) -> Vec<SearchResult> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .unwrap();

    let html = client
        .get("https://duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()
        .await
        .expect("DDG request failed")
        .text()
        .await
        .expect("DDG body failed");

    let doc = scraper::Html::parse_document(&html);
    let card_sel    = scraper::Selector::parse(".result__body").unwrap();
    let title_sel   = scraper::Selector::parse(".result__title a").unwrap();
    let snippet_sel = scraper::Selector::parse(".result__snippet").unwrap();

    let mut results = Vec::new();
    for card in doc.select(&card_sel).take(3) {
        let title_el   = card.select(&title_sel).next();
        let snippet_el = card.select(&snippet_sel).next();

        let title   = title_el.map(|el| el.text().collect::<String>().trim().to_string()).unwrap_or_default();
        let url     = title_el.and_then(|el| el.value().attr("href")).unwrap_or_default().to_string();
        let snippet = snippet_el.map(|el| el.text().collect::<String>().trim().to_string()).unwrap_or_default();

        if !title.is_empty() {
            results.push(SearchResult { title, snippet, url });
        }
    }
    results
}

// ── Page fetch ────────────────────────────────────────────────────────────────

async fn fetch_page(url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();

    let html = client.get(url).send().await.ok()?.text().await.ok()?;
    let doc = scraper::Html::parse_document(&html);
    let sel = scraper::Selector::parse("body p, body h1, body h2, body h3, body li").unwrap();

    let mut parts: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for el in doc.select(&sel) {
        let text: String = el.text().collect::<Vec<_>>().join(" ");
        let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.len() > 20 && seen.insert(t.clone()) {
            parts.push(t);
        }
    }
    let combined = parts.join("\n");
    Some(combined.chars().take(MAX_PAGE_TEXT).collect())
}

// ── Full pipeline test ────────────────────────────────────────────────────────

#[tokio::test]
async fn research_full_pipeline() {
    let topic = "AgoraMarket 平台功能分析";
    let url   = "https://agoramarket.purrtechllc.com/";

    println!("\n╔══════════════════════════════════════════╗");
    println!("║   🔬 Full Research Pipeline Integration  ║");
    println!("╚══════════════════════════════════════════╝");
    println!("  Topic: {topic}");
    println!("  URL:   {url}");

    // ── Phase 1: Fetch page ───────────────────────────────────────────────────
    println!("\n[1/5] 🌐 Fetching page...");
    let page_text = fetch_page(url).await;
    match &page_text {
        Some(t) => println!("      ✅ {} chars extracted", t.len()),
        None    => println!("      ⚠️  fetch failed, will use topic only"),
    }

    // ── Phase 2: Overview analysis ────────────────────────────────────────────
    println!("\n[2/5] 🧠 Overview analysis (LLM call 1)...");
    let context = match &page_text {
        Some(t) => format!("URL: {url}\n\nPage content:\n{}", &t.chars().take(2000).collect::<String>()),
        None    => format!("Research topic: {topic}"),
    };

    let overview_prompt = format!(
        "You are an expert analyst. Analyze the following and provide a structured overview.\n\
         Respond in Traditional Chinese.\n\
         Format:\n【是什麼】...\n【主要功能】...\n【關鍵技術】...\n\n\
         Input:\n{context}\n\nProvide your structured overview:"
    );
    let overview = llm(&overview_prompt).await;
    assert!(!overview.trim().is_empty(), "Overview LLM returned empty");
    println!("      ✅ {} chars", overview.len());
    println!("      Preview: {}...", &overview.chars().take(120).collect::<String>());

    // ── Phase 3: Generate research questions ──────────────────────────────────
    println!("\n[3/5] 💡 Generating research questions (LLM call 2)...");
    let q_prompt = format!(
        "Based on this overview, generate exactly 4 specific research questions.\n\
         Respond in Traditional Chinese. One question per line, numbered 1-4. No extra text.\n\n\
         Overview:\n{overview}\n\n4 research questions:"
    );
    let q_raw = llm(&q_prompt).await;
    let questions: Vec<String> = q_raw
        .lines()
        .filter_map(|l| {
            let t = l.trim()
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')', ' '])
                .trim()
                .to_string();
            if t.len() > 5 { Some(t) } else { None }
        })
        .take(4)
        .collect();

    assert!(!questions.is_empty(), "No questions generated");
    println!("      ✅ {} questions generated", questions.len());
    for (i, q) in questions.iter().enumerate() {
        println!("      Q{}: {}", i + 1, q);
    }

    // ── Phase 4: Search + analyse each question ───────────────────────────────
    println!("\n[4/5] 🔍 Q&A research ({} questions × search + LLM)...", questions.len());
    let mut qa_results: Vec<String> = Vec::new();

    for (i, question) in questions.iter().enumerate() {
        print!("      Q{} searching... ", i + 1);
        let results = ddg_search(question).await;
        let search_block = if results.is_empty() {
            "（外部搜尋暫時不可用，請基於既有知識回答並標示可能不完整）".to_string()
        } else {
            results.iter().take(3)
                .map(|r| format!("- {}: {} ({})", r.title, r.snippet, r.url))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let qa_prompt = format!(
            "Research question: {question}\n\nWeb search results:\n{search_block}\n\n\
             Answer in Traditional Chinese, 3-5 sentences max.\n\nAnswer:"
        );
        let answer = llm(&qa_prompt).await;
        assert!(!answer.trim().is_empty(), "Q{} LLM returned empty", i + 1);

        println!("✅ {} chars", answer.len());
        qa_results.push(format!("Q: {question}\nA: {answer}"));
    }

    // ── Phase 5: Synthesis ────────────────────────────────────────────────────
    println!("\n[5/5] 📄 Synthesising final report (LLM call {})...", 2 + questions.len() + 1);
    let all_qa   = qa_results.join("\n\n---\n\n");
    let ov_snip: String = overview.chars().take(600).collect();

    let synth_prompt = format!(
        "You are a senior analyst. Write a research report in Traditional Chinese.\n\
         Topic: {topic}\nURL: {url}\n\n\
         Overview:\n{ov_snip}\n\nResearch Q&A:\n{all_qa}\n\n\
         Report format:\n\
         【執行摘要】3 sentences\n\
         【核心發現】bullet points\n\
         【詳細分析】deeper analysis\n\
         【結論與建議】conclusions\n\nResearch report:"
    );
    let report = llm(&synth_prompt).await;
    assert!(!report.trim().is_empty(), "Synthesis LLM returned empty");
    println!("      ✅ {} chars", report.len());

    // ── Summary ───────────────────────────────────────────────────────────────
    let total_llm_calls = 2 + questions.len() + 1;
    println!("\n╔══════════════════════════════════════════╗");
    println!("║   ✅ Research pipeline completed!        ║");
    println!("╠══════════════════════════════════════════╣");
    println!("  LLM calls    : {total_llm_calls}");
    println!("  Questions    : {}", questions.len());
    println!("  Report chars : {}", report.len());
    println!("\n📄 Final report:");
    println!("──────────────────────────────────────────");
    println!("{report}");
    println!("──────────────────────────────────────────\n");
}
