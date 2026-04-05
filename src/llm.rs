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
//! | `OLLAMA_MODEL`      | `llama3.2`                     | Main model name                 |
//! | `LM_STUDIO_BASE_URL`| `http://localhost:1234/v1`     | LM Studio / OpenAI endpoint     |
//! | `LM_STUDIO_MODEL`   | `llama3.2`                     | Main model name                 |
//! | `LM_STUDIO_API_KEY` | *(empty)*                      | Optional Bearer token           |
//! | `ROUTER_MODEL`      | *(falls back to main model)*   | Small model for Router/Planner; kept resident in Ollama via `keep_alive=-1` |
//! | `CODING_MODEL`      | *(falls back to main model)*   | Dedicated model for CodingAgent |
//! | `LARGE_MODEL`       | *(falls back to main model)*   | Large model for deep reasoning  |

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

static SHARED_LLM: OnceLock<Arc<LlmConfig>> = OnceLock::new();

/// Returns the process-wide LLM config.
///
/// If [`init_shared_llm`] was called before the first use, returns the
/// probed/validated config.  Otherwise falls back to `LlmConfig::from_env()`.
pub(crate) fn shared_llm() -> Arc<LlmConfig> {
    Arc::clone(SHARED_LLM.get_or_init(|| Arc::new(LlmConfig::from_env())))
}

/// Prime the process-wide LLM singleton with a probed config.
///
/// Must be called **before** the first call to [`shared_llm`].  A second call
/// is a no-op because the underlying `OnceLock` is already set.
pub(crate) fn init_shared_llm(config: LlmConfig) {
    let _ = SHARED_LLM.set(Arc::new(config));
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
    /// Optional separate model to use for coding tasks (e.g. `qwen2.5-coder`).
    /// Falls back to `model` when not set.
    /// Set via `CODING_MODEL` environment variable.
    pub coding_model: Option<String>,
    /// Optional small/fast model for routing and planning (e.g. `tinyllama`, `phi3-mini`).
    /// Kept resident in Ollama via `keep_alive: -1`.
    /// Falls back to `model` when not set.
    /// Set via `ROUTER_MODEL` environment variable.
    pub router_model: Option<String>,
    /// Optional large/powerful model for complex reasoning tasks (e.g. `llama3:70b`).
    /// Falls back to `model` when not set.
    /// Set via `LARGE_MODEL` environment variable.
    pub large_model: Option<String>,
}

impl LlmConfig {
    /// Build config from environment variables.  Falls back to Ollama defaults.
    pub fn from_env() -> Self {
        let provider = std::env::var("LLM_PROVIDER")
            .unwrap_or_else(|_| "ollama".to_string())
            .to_lowercase();

        let coding_model = std::env::var("CODING_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty());

        let router_model = std::env::var("ROUTER_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty());

        let large_model = std::env::var("LARGE_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty());

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
                coding_model,
                router_model,
                large_model,
            },
            _ => Self {
                backend: LlmBackend::Ollama,
                base_url: std::env::var("OLLAMA_BASE_URL")
                    .unwrap_or_else(|_| OLLAMA_BASE_URL.to_string()),
                model: std::env::var("OLLAMA_MODEL")
                    .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
                api_key: None,
                coding_model,
                router_model,
                large_model,
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

    /// The model name to use for coding tasks.
    /// Falls back to the general `model` if `CODING_MODEL` is not set.
    pub fn effective_coding_model(&self) -> &str {
        self.coding_model.as_deref().unwrap_or(&self.model)
    }

    /// The model name to use for routing and planning (small/fast model).
    /// Falls back to the general `model` if `ROUTER_MODEL` is not set.
    pub fn effective_router_model(&self) -> &str {
        self.router_model.as_deref().unwrap_or(&self.model)
    }

    /// The model name to use for complex reasoning tasks (large model).
    /// Falls back to the general `model` if `LARGE_MODEL` is not set.
    pub fn effective_large_model(&self) -> &str {
        self.large_model.as_deref().unwrap_or(&self.model)
    }
}

// ── Public message types ──────────────────────────────────────────────────────

/// Role of a participant in a multi-turn conversation.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// A single turn in a multi-turn conversation sent to the LLM.
///
/// Use the convenience constructors ([`LlmMessage::system`], [`LlmMessage::user`],
/// [`LlmMessage::assistant`]) or build directly and pass a slice to
/// [`call_prompt_messages`].
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: MessageRole,
    pub content: String,
}

#[allow(dead_code)]
impl LlmMessage {
    /// Create a `system` role message.
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: MessageRole::System, content: content.into() }
    }

    /// Create a `user` role message.
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: MessageRole::User, content: content.into() }
    }

    /// Create an `assistant` role message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: MessageRole::Assistant, content: content.into() }
    }
}

// ── HTTP request / response types (private) ───────────────────────────────────

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
    keep_alive: Option<serde_json::Value>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/api/generate", base_url.trim_end_matches('/'));
    let body = OllamaRequest { model, prompt, stream: false, keep_alive };
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
    call_openai_messages(
        client,
        base_url,
        model,
        api_key,
        vec![OpenAiMessage { role: "user".into(), content: prompt }],
    )
    .await
}

/// Send a pre-built messages array to an OpenAI-compatible endpoint.
async fn call_openai_messages(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    messages: Vec<OpenAiMessage>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = OpenAiRequest { model, messages, stream: false };
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
        LlmBackend::Ollama => call_ollama(client, &llm.base_url, &llm.model, prompt, None).await,
        LlmBackend::LmStudio => {
            call_openai(client, &llm.base_url, &llm.model, llm.api_key.as_deref(), prompt).await
        }
    }
}

/// Like [`call_prompt`] but uses the coding-specific model when configured.
pub async fn call_coding_prompt(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: impl Into<String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = prompt.into();
    let model = llm.effective_coding_model();
    match llm.backend {
        LlmBackend::Ollama => call_ollama(client, &llm.base_url, model, prompt, None).await,
        LlmBackend::LmStudio => {
            call_openai(client, &llm.base_url, model, llm.api_key.as_deref(), prompt).await
        }
    }
}

/// Like [`call_prompt`] but uses the router/planner model when configured.
///
/// On Ollama, sets `keep_alive: -1` to keep the small routing model resident
/// in VRAM between calls, eliminating the model-load overhead on every request.
pub async fn call_router_prompt(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: impl Into<String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = prompt.into();
    let model = llm.effective_router_model();
    match llm.backend {
        LlmBackend::Ollama => {
            call_ollama(client, &llm.base_url, model, prompt, Some(serde_json::json!(-1))).await
        }
        LlmBackend::LmStudio => {
            call_openai(client, &llm.base_url, model, llm.api_key.as_deref(), prompt).await
        }
    }
}

/// Like [`call_prompt`] but uses the large/powerful model when configured.
pub async fn call_large_prompt(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: impl Into<String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = prompt.into();
    let model = llm.effective_large_model();
    match llm.backend {
        LlmBackend::Ollama => call_ollama(client, &llm.base_url, model, prompt, None).await,
        LlmBackend::LmStudio => {
            call_openai(client, &llm.base_url, model, llm.api_key.as_deref(), prompt).await
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

/// Send a multi-turn conversation to the LLM and return the trimmed response.
///
/// For Ollama (which uses a single-prompt API), the messages are serialised as
/// a `System: … User: … Assistant: …` string.  For OpenAI-compatible backends
/// the messages array is forwarded directly.
#[allow(dead_code)]
pub async fn call_prompt_messages(
    client: &reqwest::Client,
    llm: &LlmConfig,
    messages: &[LlmMessage],
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    match llm.backend {
        LlmBackend::Ollama => {
            // Ollama uses a flat prompt string — serialise the conversation.
            let prompt = messages
                .iter()
                .map(|m| format!("{}: {}", m.role.as_str().to_uppercase(), m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            call_ollama(client, &llm.base_url, &llm.model, prompt, None).await
        }
        LlmBackend::LmStudio => {
            let openai_msgs: Vec<OpenAiMessage> = messages
                .iter()
                .map(|m| OpenAiMessage {
                    role: m.role.as_str().to_string(),
                    content: m.content.clone(),
                })
                .collect();
            call_openai_messages(client, &llm.base_url, &llm.model, llm.api_key.as_deref(), openai_msgs).await
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
    let body = OllamaRequest { model, prompt, stream: true, keep_alive: None };
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

// ── Environment probe ─────────────────────────────────────────────────────────

/// Lightweight model descriptor returned by the backend's tag/model-list endpoint.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Canonical model name as returned by the backend (may include tag, e.g. `llama3.2:latest`).
    pub name: String,
    /// Model size in bytes (`0` when the backend does not report it).
    pub size_bytes: u64,
}

impl ModelInfo {
    /// Name without the `:latest` (or any other) tag suffix — used for matching.
    pub fn base_name(&self) -> &str {
        self.name.split(':').next().unwrap_or(&self.name)
    }

    fn name_contains_any(&self, patterns: &[&str]) -> bool {
        let lower = self.name.to_lowercase();
        patterns.iter().any(|p| lower.contains(p))
    }
}

// ── Deserialization for /api/tags (Ollama) ────────────────────────────────────

#[derive(Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaTagEntry>,
}

#[derive(Deserialize)]
struct OllamaTagEntry {
    name: String,
    #[serde(default)]
    size: u64,
}

// ── Deserialization for GET /v1/models (LM Studio / OpenAI) ──────────────────

#[derive(Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelEntry>,
}

#[derive(Deserialize)]
struct OpenAiModelEntry {
    id: String,
}

// ── Model listing ─────────────────────────────────────────────────────────────

/// Query the Ollama `/api/tags` endpoint and return the available model list.
/// Returns an empty `Vec` (non-fatal) on any network or parse error.
async fn list_ollama_models(
    client: &reqwest::Client,
    base_url: &str,
) -> Vec<ModelInfo> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            resp.json::<OllamaTagsResponse>()
                .await
                .map(|r| {
                    r.models
                        .into_iter()
                        .map(|e| ModelInfo { name: e.name, size_bytes: e.size })
                        .collect()
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// Query the LM Studio `/v1/models` endpoint and return the available model list.
/// Returns an empty `Vec` (non-fatal) on any network or parse error.
async fn list_lmstudio_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> Vec<ModelInfo> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5));
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            resp.json::<OpenAiModelsResponse>()
                .await
                .map(|r| {
                    r.data
                        .into_iter()
                        .map(|e| ModelInfo { name: e.id, size_bytes: 0 })
                        .collect()
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

// ── Role-assignment heuristics ────────────────────────────────────────────────

/// Return the first model whose name contains any of `patterns` (case-insensitive).
fn find_by_name(models: &[ModelInfo], patterns: &[&str]) -> Option<String> {
    models
        .iter()
        .find(|m| m.name_contains_any(patterns))
        .map(|m| m.name.clone())
}

/// Smallest model by reported size (excluding the main model).
fn find_smallest(models: &[ModelInfo], exclude: &str) -> Option<String> {
    models
        .iter()
        .filter(|m| m.size_bytes > 0 && m.base_name() != exclude)
        .min_by_key(|m| m.size_bytes)
        .map(|m| m.name.clone())
}

/// Largest model by reported size.
fn find_largest(models: &[ModelInfo]) -> Option<String> {
    models
        .iter()
        .filter(|m| m.size_bytes > 0)
        .max_by_key(|m| m.size_bytes)
        .map(|m| m.name.clone())
}

/// Returns `Some(name)` if `name` is confirmed present in `models`.
fn validate_model<'a>(name: &'a str, models: &[ModelInfo]) -> Option<&'a str> {
    let lower = name.to_lowercase();
    let base = lower.split(':').next().unwrap_or(&lower);
    models.iter().find(|m| {
        let mn = m.name.to_lowercase();
        let mb = m.base_name().to_lowercase();
        mn == lower || mb == lower || mn == base || mb == base
    }).map(|_| name)
}

// ── Assignment logic ──────────────────────────────────────────────────────────

/// Resolve a single model role:
/// - If env supplied a name and it's confirmed available → keep it.
/// - If env supplied a name but it's absent → warn, clear the role.
/// - If env supplied nothing → use `auto_detect`, unless it duplicates `main_model`.
fn assign_role(
    role: &str,
    env_value: Option<String>,
    models: &[ModelInfo],
    auto_detect: Option<String>,
    main_model: &str,
) -> Option<String> {
    match env_value {
        Some(ref name) if !name.is_empty() => {
            if validate_model(name, models).is_some() {
                Some(name.clone())
            } else {
                eprintln!(
                    "[llm probe] WARNING: {role} model '{name}' not found in backend — \
                     falling back to main model '{main_model}'"
                );
                None
            }
        }
        _ => auto_detect.filter(|n| n.as_str() != main_model),
    }
}

// ── Public probe entry point ──────────────────────────────────────────────────

/// Probe the configured LLM backend at startup, validate env-specified model
/// names against the live model list, and auto-assign unset roles using
/// name/size heuristics.
///
/// Returns the env-only baseline on any network or parse error (non-fatal).
///
/// ## Role assignment priority
/// 1. Env var set **and** model confirmed present → use as-is.
/// 2. Env var set **but** model absent → warn + clear the role (falls back to
///    main model at call time via `effective_*_model()`).
/// 3. Env var not set → heuristic auto-assign from available models.
pub async fn probe_and_configure(client: &reqwest::Client) -> LlmConfig {
    let baseline = LlmConfig::from_env();

    let models: Vec<ModelInfo> = match baseline.backend {
        LlmBackend::Ollama => list_ollama_models(client, &baseline.base_url).await,
        LlmBackend::LmStudio => {
            list_lmstudio_models(client, &baseline.base_url, baseline.api_key.as_deref()).await
        }
    };

    if models.is_empty() {
        eprintln!(
            "[llm probe] {} at '{}' returned no models — using env-only config",
            baseline.backend_name(),
            baseline.base_url,
        );
        return baseline;
    }

    eprintln!(
        "[llm probe] {} model(s) available on {} ({})",
        models.len(),
        baseline.backend_name(),
        baseline.base_url,
    );
    for m in &models {
        let size_str = if m.size_bytes > 0 {
            format!(" [{:.1} GB]", m.size_bytes as f64 / 1_073_741_824.0)
        } else {
            String::new()
        };
        eprintln!("  • {}{}", m.name, size_str);
    }

    // ── Validate main model ───────────────────────────────────────────────────
    let main_model = if validate_model(&baseline.model, &models).is_some() {
        baseline.model.clone()
    } else {
        let fallback = models.first().map(|m| m.name.clone()).unwrap_or(baseline.model.clone());
        if fallback != baseline.model {
            eprintln!(
                "[llm probe] WARNING: main model '{}' not found — using first available '{fallback}'",
                baseline.model
            );
        }
        fallback
    };

    // ── Heuristic candidates for unset roles ─────────────────────────────────

    // Router / planner: prefer small, fast models.
    let auto_router = find_by_name(&models, &[
        "tinyllama", "phi3-mini", "phi-3-mini", "phi3:mini", "phi",
        "qwen:0.5", "qwen:1.5", "qwen2:0.5", "qwen2:1.5",
        "smollm", "gemma:2b",
    ])
    .or_else(|| find_smallest(&models, &main_model));

    // Coding: prefer dedicated code models.
    let auto_coding = find_by_name(&models, &[
        "qwen2.5-coder", "qwen2.5coder", "codellama", "starcoder",
        "deepseek-coder", "devstral", "coder", "code",
    ]);

    // Large: prefer the biggest model by size or well-known large model names.
    let auto_large = find_by_name(&models, &[
        "70b", "72b", "65b", "34b", "32b", "mixtral", "opus",
    ])
    .or_else(|| find_largest(&models));

    // ── Apply assignments ─────────────────────────────────────────────────────
    let router_model = assign_role("router", baseline.router_model, &models, auto_router, &main_model);
    let coding_model = assign_role("coding", baseline.coding_model, &models, auto_coding, &main_model);
    let large_model  = assign_role("large",  baseline.large_model,  &models, auto_large,  &main_model);

    eprintln!("[llm probe] Model assignments:");
    eprintln!("  main   → {main_model}");
    eprintln!("  router → {}", router_model.as_deref().unwrap_or("(same as main)"));
    eprintln!("  coding → {}", coding_model.as_deref().unwrap_or("(same as main)"));
    eprintln!("  large  → {}", large_model.as_deref().unwrap_or("(same as main)"));

    LlmConfig {
        backend:      baseline.backend,
        base_url:     baseline.base_url,
        model:        main_model,
        api_key:      baseline.api_key,
        router_model,
        coding_model,
        large_model,
    }
}
