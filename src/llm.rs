//! Shared LLM provider abstraction — Ollama and OpenAI-compatible (LM Studio) backends.
//!
//! Both [`crate::telegram`] and [`crate::followup`] and [`crate::researcher`]
//! use the same environment variables and HTTP protocol; this module avoids
//! duplicating that logic across three files.
//!
//! ## Environment variables
//!
//! | Variable            | Default                        | Description                     |
//! |---------------------|--------------------------------|---------------------------------|
//! | `LLM_PROVIDER`      | `ollama`                       | `ollama` or `lmstudio`/`openai` |
//! | `OLLAMA_BASE_URL`   | `http://localhost:11434`       | Ollama server address           |
//! | `OLLAMA_MODEL`      | `llama3.2`                     | Model name                      |
//! | `LM_STUDIO_BASE_URL`| `http://localhost:1234/v1`     | LM Studio / OpenAI endpoint     |
//! | `LM_STUDIO_MODEL`   | `llama3.2`                     | Model name                      |
//! | `LM_STUDIO_API_KEY` | *(empty)*                      | Optional Bearer token           |

use std::sync::{Arc, OnceLock};

use futures::StreamExt;
use serde::{Deserialize, Serialize};

const OLLAMA_BASE_URL: &str = "http://localhost:11434";

// ── Process-wide singletons ───────────────────────────────────────────────────

/// Returns the process-wide shared HTTP client (initialized once).
pub(crate) fn shared_http() -> Arc<reqwest::Client> {
    static HTTP: OnceLock<Arc<reqwest::Client>> = OnceLock::new();
    Arc::clone(HTTP.get_or_init(|| Arc::new(reqwest::Client::new())))
}

/// Returns the process-wide LLM config (read from env once).
pub(crate) fn shared_llm() -> Arc<LlmConfig> {
    static LLM: OnceLock<Arc<LlmConfig>> = OnceLock::new();
    Arc::clone(LLM.get_or_init(|| Arc::new(LlmConfig::from_env())))
}
const LM_STUDIO_BASE_URL: &str = "http://localhost:1234/v1";
const DEFAULT_MODEL: &str = "llama3.2";

// ── Backend enum ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmBackend {
    Ollama,
    LmStudio,
}

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub backend: LlmBackend,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

impl LlmConfig {
    /// Build config from environment variables.  Falls back to Ollama defaults.
    pub fn from_env() -> Self {
        let provider = std::env::var("LLM_PROVIDER")
            .unwrap_or_else(|_| "ollama".to_string())
            .to_lowercase();

        match provider.as_str() {
            "lmstudio" | "lm_studio" | "openai" => Self {
                backend: LlmBackend::LmStudio,
                base_url: std::env::var("LM_STUDIO_BASE_URL")
                    .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                    .unwrap_or_else(|_| LM_STUDIO_BASE_URL.to_string()),
                model: std::env::var("LM_STUDIO_MODEL")
                    .or_else(|_| std::env::var("OPENAI_MODEL"))
                    .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
                api_key: std::env::var("LM_STUDIO_API_KEY")
                    .or_else(|_| std::env::var("OPENAI_API_KEY"))
                    .ok()
                    .filter(|v| !v.trim().is_empty()),
            },
            _ => Self {
                backend: LlmBackend::Ollama,
                base_url: std::env::var("OLLAMA_BASE_URL")
                    .unwrap_or_else(|_| OLLAMA_BASE_URL.to_string()),
                model: std::env::var("OLLAMA_MODEL")
                    .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
                api_key: None,
            },
        }
    }

    /// Short label for logging (e.g. `"ollama"` or `"lmstudio"`).
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            LlmBackend::Ollama => "ollama",
            LlmBackend::LmStudio => "lmstudio",
        }
    }
}

// ── HTTP request / response types (private) ───────────────────────────────────

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    prompt: String,
    stream: bool,
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

#[derive(Serialize, Deserialize)]
struct OpenAiMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

async fn call_ollama(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: String,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/api/generate", base_url.trim_end_matches('/'));
    let body = OllamaRequest { model, prompt, stream: false };
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

async fn call_openai(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    prompt: String,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = OpenAiRequest {
        model,
        messages: vec![OpenAiMessage { role: "user".into(), content: prompt }],
        stream: false,
    };
    let mut req = client.post(&url).json(&body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    let resp: OpenAiResponse = req.send().await?.error_for_status()?.json().await?;
    Ok(resp.choices.first().map(|c| c.message.content.trim().to_string()).unwrap_or_default())
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Send `prompt` to the configured LLM backend and return the trimmed response.
pub async fn call_prompt(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: impl Into<String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = prompt.into();
    match llm.backend {
        LlmBackend::Ollama => call_ollama(client, &llm.base_url, &llm.model, prompt).await,
        LlmBackend::LmStudio => {
            call_openai(client, &llm.base_url, &llm.model, llm.api_key.as_deref(), prompt).await
        }
    }
}

/// Stream `prompt` to the LLM backend.
///
/// `on_token` is called for every token received.  Returns the full
/// concatenated response when the stream ends.
///
/// Falls back to a blocking call if streaming is unavailable.
pub async fn call_prompt_stream<F>(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: impl Into<String>,
    on_token: F,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(String) + Send,
{
    let prompt = prompt.into();
    match llm.backend {
        LlmBackend::Ollama => {
            stream_ollama(client, &llm.base_url, &llm.model, prompt, on_token).await
        }
        LlmBackend::LmStudio => {
            stream_openai(
                client,
                &llm.base_url,
                &llm.model,
                llm.api_key.as_deref(),
                prompt,
                on_token,
            )
            .await
        }
    }
}

async fn stream_ollama<F>(
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
    let body = OllamaRequest { model, prompt, stream: true };
    let resp = client.post(&url).json(&body).send().await?.error_for_status()?;

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
                if line.is_empty() { continue; }
                if let Ok(chunk) = serde_json::from_str::<OllamaStreamChunk>(line) {
                    if !chunk.response.is_empty() {
                        on_token(chunk.response.clone());
                        full.push_str(&chunk.response);
                    }
                    if chunk.done { break; }
                }
            }
        }
    }

    Ok(full.trim().to_string())
}

async fn stream_openai<F>(
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
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = OpenAiStreamRequest {
        model,
        messages: vec![OpenAiMessage { role: "user".into(), content: prompt }],
        stream: true,
    };
    let mut req = client.post(&url).json(&body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?.error_for_status()?;

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
                    if data == "[DONE]" || data.is_empty() { continue; }
                    if let Ok(ch) = serde_json::from_str::<OpenAiStreamChunk>(data) {
                        if let Some(content) = ch.choices.first()
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
