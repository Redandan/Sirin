/// Integration test: full pipeline
/// receive message → should_search → ddg_search → build_prompt → LM Studio → reply
///
/// Run with:
///   cargo test pipeline -- --nocapture

use serde::{Deserialize, Serialize};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

const LM_STUDIO_URL: &str = "http://localhost:1234/v1/chat/completions";
const LM_STUDIO_MODEL: &str = "llama-3.2-3b-instruct-uncensored";

// ── LM Studio types ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    stream: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

// ── DuckDuckGo search ─────────────────────────────────────────────────────────

#[derive(Debug)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
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
        .expect("HTTP request failed")
        .text()
        .await
        .expect("Failed to read body");

    let document = scraper::Html::parse_document(&html);
    let result_sel = scraper::Selector::parse(".result__body").unwrap();
    let title_sel  = scraper::Selector::parse(".result__title a").unwrap();
    let snippet_sel = scraper::Selector::parse(".result__snippet").unwrap();

    let mut results = Vec::new();
    for card in document.select(&result_sel).take(3) {
        let title_el   = card.select(&title_sel).next();
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

        if !title.is_empty() {
            results.push(SearchResult { title, url, snippet });
        }
    }
    results
}

// ── should_search heuristic ───────────────────────────────────────────────────

fn should_search(text: &str) -> bool {
    let lower = text.to_lowercase();
    text.contains('?') || text.contains('？')
        || lower.contains("什麼") || lower.contains("如何")
        || lower.contains("為什麼") || lower.contains("怎麼")
        || lower.contains("what") || lower.contains("how")
        || lower.contains("why")  || lower.contains("when")
        || lower.contains("where")|| lower.contains("who")
}

// ── LM Studio call ────────────────────────────────────────────────────────────

async fn call_llm(prompt: &str) -> String {
    let client = reqwest::Client::new();
    let body = ChatRequest {
        model: LM_STUDIO_MODEL,
        messages: vec![ChatMessage { role: "user".into(), content: prompt.into() }],
        stream: false,
    };
    let resp: ChatResponse = client
        .post(LM_STUDIO_URL)
        .json(&body)
        .send()
        .await
        .expect("LM Studio request failed")
        .json()
        .await
        .expect("Failed to parse LM Studio response");

    resp.choices
        .first()
        .map(|c| c.message.content.trim().to_string())
        .unwrap_or_default()
}

// ── Full pipeline test ────────────────────────────────────────────────────────

#[tokio::test]
async fn pipeline_receive_search_think_reply() {
    // ── Step 1: Simulate incoming Telegram message ────────────────────────────
    let incoming_msg = "Rust 語言的 async/await 是怎麼運作的？";

    println!("\n======================================");
    println!("📩 收到訊息");
    println!("   > {}", incoming_msg);
    println!("======================================");

    // ── Step 2: Decide whether to search ─────────────────────────────────────
    let needs_search = should_search(incoming_msg);
    println!("\n🔍 觸發搜尋判斷 → {}", if needs_search { "YES" } else { "NO" });
    assert!(needs_search, "此訊息應該觸發搜尋");

    // ── Step 3: Web search ────────────────────────────────────────────────────
    println!("\n🌐 觸發 DuckDuckGo 搜尋...");
    let results = ddg_search(incoming_msg).await;

    if results.is_empty() {
        println!("   ⚠️  外部搜尋暫時不可用，改用模型既有知識繼續流程");
    } else {
        println!("   找到 {} 筆結果：", results.len());
        for (i, r) in results.iter().enumerate() {
            println!("   [{}] {}", i + 1, r.title);
            println!("       {}", r.snippet);
            println!("       {}", r.url);
        }
    }

    let search_block = if results.is_empty() {
        "- External search unavailable; answer from model knowledge and say when uncertain.".to_string()
    } else {
        results
            .iter()
            .map(|r| format!("- {}: {} ({})", r.title, r.snippet, r.url))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // ── Step 4: Build prompt ──────────────────────────────────────────────────
    let prompt = format!(
        "You are Sirin, a natural and helpful AI assistant.\n\
         Reply in the same language as the user message.\n\
         Keep response concise (2-3 sentences).\n\
         \n\
         User message: {incoming_msg}\n\
         \n\
         Web search results (use as reference):\n{search_block}\n\
         \n\
         Return only the final reply text."
    );

    println!("\n🧠 觸發 LLM 思考...");
    println!("   Prompt 長度: {} chars", prompt.len());

    // ── Step 5: LLM generate reply ────────────────────────────────────────────
    let reply = call_llm(&prompt).await;
    assert!(!reply.trim().is_empty(), "LLM 應該返回非空回覆");

    println!("\n💬 生成回覆：");
    println!("--------------------------------------");
    println!("{}", reply);
    println!("--------------------------------------");
    println!("\n✅ 完整流程測試通過\n");
}
