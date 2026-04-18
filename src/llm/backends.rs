//! HTTP wire types and transport functions for the Ollama and OpenAI-compatible
//! backends.  The public `call_prompt*` / `call_prompt_stream` functions in
//! [`super`] dispatch to these based on [`super::LlmBackend`].
//!
//! Streaming implementations handle both Ollama's one-JSON-per-newline format
//! and OpenAI's SSE `data: {…}\n\n` framing.  429 rate-limit responses are
//! retried up to 3 times with exponential back-off.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

// ── HTTP request / response types (private to this module) ───────────────────

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    prompt: String,
    stream: bool,
    /// Controls how long Ollama keeps the model loaded after the request.
    /// Use `json!(-1)` to keep the model resident permanently (ideal for the
    /// small routing model), or a duration string like `"5m"` to unload it
    /// after a period of inactivity.  `None` uses the Ollama server default.
    /// Ignored by LM Studio / OpenAI-compatible backends.
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct OllamaStreamChunk {
    #[serde(default)]
    response: String,
    #[serde(default)]
    done: bool,
}

#[derive(Serialize)]
struct OpenAiStreamRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    stream: bool,
}

#[derive(Deserialize)]
struct OpenAiStreamChunk {
    choices: Vec<OpenAiStreamChoice>,
}

#[derive(Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiDelta,
}

#[derive(Deserialize, Default)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    stream: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub(super) struct OpenAiMessage {
    pub role: String,
    /// Text string OR multimodal content array (for vision).
    pub content: serde_json::Value,
}

impl OpenAiMessage {
    /// Create a text-only message.
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: serde_json::Value::String(content.into()) }
    }

    /// Create a message with text + image (base64 PNG).
    pub fn with_image(role: &str, text: &str, image_base64: &str, mime: &str) -> Self {
        Self {
            role: role.into(),
            content: serde_json::json!([
                { "type": "text", "text": text },
                { "type": "image_url", "image_url": {
                    "url": format!("data:{mime};base64,{image_base64}")
                }}
            ]),
        }
    }

    /// Extract text content (whether string or array).
    pub fn text_content(&self) -> String {
        match &self.content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => {
                arr.iter()
                    .filter_map(|part| {
                        if part.get("type")?.as_str()? == "text" {
                            part.get("text")?.as_str().map(|s| s.to_string())
                        } else { None }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => String::new(),
        }
    }
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

// ── Non-streaming transport ──────────────────────────────────────────────────

pub(super) async fn call_ollama(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: String,
    keep_alive: Option<serde_json::Value>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    crate::sirin_log!(
        "[llm] call  backend=ollama model={} chars={}",
        model,
        prompt.len()
    );
    let url = format!("{}/api/generate", base_url.trim_end_matches('/'));
    let body = OllamaRequest {
        model,
        prompt,
        stream: false,
        keep_alive,
    };
    let resp: OllamaResponse = client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.response.trim().to_string())
}

pub(super) async fn call_openai(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    prompt: String,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    call_openai_messages(
        client,
        base_url,
        model,
        api_key,
        vec![OpenAiMessage::text("user", prompt)],
    )
    .await
}

/// Send a pre-built messages array to an OpenAI-compatible endpoint.
/// Retries up to 3 times on HTTP 429 (Too Many Requests) with exponential back-off
/// (30 s → 60 s → 120 s), honouring the `Retry-After` response header when present.
pub(super) async fn call_openai_messages(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    messages: Vec<OpenAiMessage>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let total_chars: usize = messages.iter().map(|m| m.text_content().len()).sum();
    crate::sirin_log!(
        "[llm] call  backend=openai-compat model={} msgs={} chars={}",
        model,
        messages.len(),
        total_chars
    );
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = OpenAiRequest {
        model,
        messages,
        stream: false,
    };

    let mut attempt = 0u32;
    loop {
        let mut req = client.post(&url).json(&body);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if attempt >= 3 {
                crate::sirin_log!("[llm] 429 max retries exceeded model={}", model);
                return Err(resp.error_for_status().unwrap_err().into());
            }
            let wait_secs = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30u64 << attempt); // 30 → 60 → 120
            crate::sirin_log!(
                "[llm] 429 rate-limited — waiting {}s (attempt {}/3) model={}",
                wait_secs,
                attempt + 1,
                model
            );
            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            attempt += 1;
            continue;
        }
        let parsed: OpenAiResponse = resp.error_for_status()?.json().await?;
        let reply = parsed
            .choices
            .first()
            .map(|c| c.message.text_content().trim().to_string())
            .unwrap_or_default();
        crate::sirin_log!(
            "[llm] resp  backend=openai-compat model={} reply_chars={}",
            model,
            reply.len()
        );
        return Ok(reply);
    }
}

// ── Streaming transport ──────────────────────────────────────────────────────

pub(super) async fn stream_ollama<F>(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: String,
    on_token: F,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(String) + Send,
{
    let url = format!("{}/api/generate", base_url.trim_end_matches('/'));
    let body = OllamaRequest {
        model,
        prompt,
        stream: true,
        keep_alive: None,
    };
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;

    let mut stream = resp.bytes_stream();
    let mut full = String::new();
    let mut buf = Vec::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.extend_from_slice(&bytes);

        // Ollama sends one JSON object per newline.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes = buf.drain(..=pos).collect::<Vec<_>>();
            if let Ok(line) = std::str::from_utf8(&line_bytes) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(chunk) = serde_json::from_str::<OllamaStreamChunk>(line) {
                    if !chunk.response.is_empty() {
                        on_token(chunk.response.clone());
                        full.push_str(&chunk.response);
                    }
                    if chunk.done {
                        break;
                    }
                }
            }
        }
    }

    Ok(full.trim().to_string())
}

pub(super) async fn stream_openai<F>(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    prompt: String,
    on_token: F,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(String) + Send,
{
    crate::sirin_log!(
        "[llm] stream backend=openai-compat model={} chars={}",
        model,
        prompt.len()
    );
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = OpenAiStreamRequest {
        model,
        messages: vec![OpenAiMessage::text("user", prompt)],
        stream: true,
    };

    let mut attempt = 0u32;
    let resp = loop {
        let mut req = client.post(&url).json(&body);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        let r = req.send().await?;
        if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if attempt >= 3 {
                crate::sirin_log!("[llm] 429 max retries exceeded model={} (stream)", model);
                return Err(r.error_for_status().unwrap_err().into());
            }
            let wait_secs = r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30u64 << attempt);
            crate::sirin_log!(
                "[llm] 429 rate-limited — waiting {}s (attempt {}/3) model={} (stream)",
                wait_secs,
                attempt + 1,
                model
            );
            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            attempt += 1;
            continue;
        }
        break r.error_for_status()?;
    };

    let mut stream = resp.bytes_stream();
    let mut full = String::new();
    let mut buf = Vec::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.extend_from_slice(&bytes);

        // OpenAI SSE: each message is "data: <json>\n\n" or "data: [DONE]\n\n".
        while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
            let line_bytes = buf.drain(..pos + 2).collect::<Vec<_>>();
            if let Ok(line) = std::str::from_utf8(&line_bytes) {
                for line in line.lines() {
                    let data = line.trim_start_matches("data:").trim();
                    if data == "[DONE]" || data.is_empty() {
                        continue;
                    }
                    if let Ok(ch) = serde_json::from_str::<OpenAiStreamChunk>(data) {
                        if let Some(content) = ch
                            .choices
                            .first()
                            .and_then(|c| c.delta.content.as_deref())
                            .filter(|s| !s.is_empty())
                        {
                            on_token(content.to_string());
                            full.push_str(content);
                        }
                    }
                }
            }
        }
    }

    Ok(full.trim().to_string())
}
