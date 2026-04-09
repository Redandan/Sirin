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
    Arc::clone(SHARED_LLM.get_or_init(|| {
        let mut cfg = LlmConfig::from_env();
        cfg.apply_yaml_overrides();
        crate::sirin_log!(
            "[llm] main  backend={} model={}",
            cfg.backend_name(),
            cfg.model
        );
        Arc::new(cfg)
    }))
}

/// Prime the process-wide LLM singleton with a probed config.
///
/// Must be called **before** the first call to [`shared_llm`].  A second call
/// is a no-op because the underlying `OnceLock` is already set.
pub(crate) fn init_shared_llm(config: LlmConfig) {
    crate::sirin_log!(
        "[llm] main  backend={} model={}",
        config.backend_name(),
        config.model
    );
    let _ = SHARED_LLM.set(Arc::new(config));
}

/// Returns a pre-configured `Arc<LlmConfig>` whose `model` field is already
/// set to the effective large model.  Cached process-wide so the clone happens
/// at most once.
///
/// Used by `ChatAgent` when `use_large_model = true` to avoid cloning the config
/// on every request.
pub(crate) fn shared_large_llm() -> Arc<LlmConfig> {
    static LARGE: OnceLock<Arc<LlmConfig>> = OnceLock::new();
    Arc::clone(LARGE.get_or_init(|| {
        let base = shared_llm();
        let large_model = base.effective_large_model().to_string();
        if large_model == base.model {
            // No dedicated large model configured — reuse the same Arc.
            Arc::clone(&base)
        } else {
            let mut cfg = (*base).clone();
            cfg.model = large_model;
            Arc::new(cfg)
        }
    }))
}

/// Returns the process-wide LLM config used for routing and planning.
///
/// Reads `ROUTER_LLM_PROVIDER` first.  When set, builds a config from that
/// provider (Ollama or LM Studio) so cheap classification calls stay local
/// even when the main backend is a remote service like Gemini.
/// Falls back to [`shared_llm`] when `ROUTER_LLM_PROVIDER` is not set.
pub(crate) fn shared_router_llm() -> Arc<LlmConfig> {
    static ROUTER: OnceLock<Arc<LlmConfig>> = OnceLock::new();
    Arc::clone(ROUTER.get_or_init(|| {
        let cfg = LlmConfig::router_from_env();
        crate::sirin_log!(
            "[llm] router backend={} model={}",
            cfg.backend_name(),
            cfg.model
        );
        Arc::new(cfg)
    }))
}
const LM_STUDIO_BASE_URL: &str = "http://localhost:1234/v1";
const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
const DEFAULT_MODEL: &str = "llama3.2";
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.0-flash";

// ── UI-editable LLM config (persisted to config/llm.yaml) ────────────────────

/// Stores the user's model-role assignments.
/// Saved to `config/llm.yaml`; loaded on startup to override env-var defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmUiConfig {
    /// "ollama" | "lmstudio" | "gemini"  (empty = keep env default)
    #[serde(default)]
    pub provider: String,
    /// Base URL for the backend  (empty = keep env default)
    #[serde(default)]
    pub base_url: String,
    /// Main chat model  (empty = keep env default)
    #[serde(default)]
    pub main_model: String,
    /// Router/planner model  (empty = use main)
    #[serde(default)]
    pub router_model: String,
    /// Coding model  (empty = use main)
    #[serde(default)]
    pub coding_model: String,
    /// Large/reasoning model  (empty = use main)
    #[serde(default)]
    pub large_model: String,
}

impl LlmUiConfig {
    const PATH: &'static str = "config/llm.yaml";

    pub fn load() -> Self {
        std::fs::read_to_string(Self::PATH)
            .ok()
            .and_then(|s| serde_yaml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let content = serde_yaml::to_string(self)?;
        std::fs::write(Self::PATH, content)?;
        Ok(())
    }
}

// ── Backend enum ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmBackend {
    Ollama,
    LmStudio,
    /// Google Gemini via the OpenAI-compatible endpoint at
    /// `https://generativelanguage.googleapis.com/v1beta/openai`.
    /// Set `LLM_PROVIDER=gemini` and `GEMINI_API_KEY=<key>`.
    Gemini,
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
            "gemini" | "google" => Self {
                backend: LlmBackend::Gemini,
                base_url: std::env::var("GEMINI_BASE_URL")
                    .unwrap_or_else(|_| GEMINI_BASE_URL.to_string()),
                model: std::env::var("GEMINI_MODEL")
                    .unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string()),
                api_key: std::env::var("GEMINI_API_KEY")
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
                model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
                api_key: None,
                coding_model,
                router_model,
                large_model,
            },
        }
    }

    /// Apply non-empty overrides from `config/llm.yaml` (saved by the settings UI).
    ///
    /// Fields left blank in the YAML are silently ignored so env-var defaults remain active.
    pub fn apply_yaml_overrides(&mut self) {
        let ui = LlmUiConfig::load();
        if !ui.provider.is_empty() {
            match ui.provider.to_lowercase().as_str() {
                "lmstudio" | "lm_studio" | "openai" => self.backend = LlmBackend::LmStudio,
                "gemini" | "google" => self.backend = LlmBackend::Gemini,
                "ollama" => self.backend = LlmBackend::Ollama,
                _ => {}
            }
        }
        if !ui.base_url.is_empty() {
            self.base_url = ui.base_url;
        }
        if !ui.main_model.is_empty() {
            self.model = ui.main_model;
        }
        if !ui.router_model.is_empty() {
            self.router_model = Some(ui.router_model);
        }
        if !ui.coding_model.is_empty() {
            self.coding_model = Some(ui.coding_model);
        }
        if !ui.large_model.is_empty() {
            self.large_model = Some(ui.large_model);
        }
    }

    /// Build the router/planner LLM config from environment variables.
    ///
    /// Checks `ROUTER_LLM_PROVIDER` first.  When present, constructs a config
    /// for that local backend using the same URL/model env vars as the main
    /// provider (e.g. `LM_STUDIO_BASE_URL`, `OLLAMA_MODEL`).  This lets you
    /// keep cheap intent-classification calls on a local model while routing
    /// main responses through a remote service.
    ///
    /// Falls back to `LlmConfig::from_env()` when `ROUTER_LLM_PROVIDER` is
    /// absent so existing setups require no config changes.
    pub fn router_from_env() -> Self {
        let provider = std::env::var("ROUTER_LLM_PROVIDER")
            .unwrap_or_default()
            .to_lowercase();

        match provider.as_str() {
            "lmstudio" | "lm_studio" | "openai" => Self {
                backend: LlmBackend::LmStudio,
                base_url: std::env::var("LM_STUDIO_BASE_URL")
                    .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                    .unwrap_or_else(|_| LM_STUDIO_BASE_URL.to_string()),
                model: std::env::var("ROUTER_MODEL")
                    .or_else(|_| std::env::var("LM_STUDIO_MODEL"))
                    .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
                api_key: std::env::var("LM_STUDIO_API_KEY")
                    .ok()
                    .filter(|v| !v.trim().is_empty()),
                coding_model: None,
                router_model: None,
                large_model: None,
            },
            "ollama" => Self {
                backend: LlmBackend::Ollama,
                base_url: std::env::var("OLLAMA_BASE_URL")
                    .unwrap_or_else(|_| OLLAMA_BASE_URL.to_string()),
                model: std::env::var("ROUTER_MODEL")
                    .or_else(|_| std::env::var("OLLAMA_MODEL"))
                    .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
                api_key: None,
                coding_model: None,
                router_model: None,
                large_model: None,
            },
            // No override set — fall back to the main provider config.
            _ => Self::from_env(),
        }
    }

    /// Short label for logging (e.g. `"ollama"` or `"lmstudio"`).
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            LlmBackend::Ollama => "ollama",
            LlmBackend::LmStudio => "lmstudio",
            LlmBackend::Gemini => "gemini",
        }
    }

    /// Returns true when this config points to a cloud/remote backend (Gemini).
    pub fn is_remote(&self) -> bool {
        self.backend == LlmBackend::Gemini
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

    fn effective_router_backend_name(&self) -> &'static str {
        let provider = std::env::var("ROUTER_LLM_PROVIDER")
            .unwrap_or_default()
            .to_lowercase();

        match provider.as_str() {
            "lmstudio" | "lm_studio" | "openai" => "lmstudio",
            "ollama" => "ollama",
            "gemini" => "gemini",
            _ => self.backend_name(),
        }
    }

    /// Concise human-readable summary for task-start logging.
    pub fn task_log_summary(&self) -> String {
        format!(
            "backend={} chat={}/{} router={}/{} coding={}/{} large={}/{}",
            self.backend_name(),
            self.backend_name(),
            self.model,
            self.effective_router_backend_name(),
            self.effective_router_model(),
            self.backend_name(),
            self.effective_coding_model(),
            self.backend_name(),
            self.effective_large_model(),
        )
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
        Self {
            role: MessageRole::System,
            content: content.into(),
        }
    }

    /// Create a `user` role message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
        }
    }

    /// Create an `assistant` role message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
        }
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
        vec![OpenAiMessage {
            role: "user".into(),
            content: prompt,
        }],
    )
    .await
}

/// Send a pre-built messages array to an OpenAI-compatible endpoint.
/// Retries up to 3 times on HTTP 429 (Too Many Requests) with exponential back-off
/// (30 s → 60 s → 120 s), honouring the `Retry-After` response header when present.
async fn call_openai_messages(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    messages: Vec<OpenAiMessage>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let total_chars: usize = messages.iter().map(|m| m.content.len()).sum();
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
            .map(|c| c.message.content.trim().to_string())
            .unwrap_or_default();
        crate::sirin_log!(
            "[llm] resp  backend=openai-compat model={} reply_chars={}",
            model,
            reply.len()
        );
        return Ok(reply);
    }
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
        LlmBackend::LmStudio | LlmBackend::Gemini => {
            call_openai(
                client,
                &llm.base_url,
                &llm.model,
                llm.api_key.as_deref(),
                prompt,
            )
            .await
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
        LlmBackend::LmStudio | LlmBackend::Gemini => {
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
            call_ollama(
                client,
                &llm.base_url,
                model,
                prompt,
                Some(serde_json::json!(-1)),
            )
            .await
        }
        LlmBackend::LmStudio | LlmBackend::Gemini => {
            call_openai(client, &llm.base_url, model, llm.api_key.as_deref(), prompt).await
        }
    }
}

/// Like [`call_prompt`] but uses the large/powerful model when configured.
#[allow(dead_code)]
pub async fn call_large_prompt(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: impl Into<String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = prompt.into();
    let model = llm.effective_large_model();
    match llm.backend {
        LlmBackend::Ollama => call_ollama(client, &llm.base_url, model, prompt, None).await,
        LlmBackend::LmStudio | LlmBackend::Gemini => {
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
        LlmBackend::LmStudio | LlmBackend::Gemini => {
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
        LlmBackend::LmStudio | LlmBackend::Gemini => {
            let openai_msgs: Vec<OpenAiMessage> = messages
                .iter()
                .map(|m| OpenAiMessage {
                    role: m.role.as_str().to_string(),
                    content: m.content.clone(),
                })
                .collect();
            call_openai_messages(
                client,
                &llm.base_url,
                &llm.model,
                llm.api_key.as_deref(),
                openai_msgs,
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
    crate::sirin_log!(
        "[llm] stream backend=openai-compat model={} chars={}",
        model,
        prompt.len()
    );
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = OpenAiStreamRequest {
        model,
        messages: vec![OpenAiMessage {
            role: "user".into(),
            content: prompt,
        }],
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
    /// Name without its tag suffix (e.g., `:latest`, `:13b`, `:q4_0`) — used for matching.
    pub fn base_name(&self) -> &str {
        self.name.split(':').next().unwrap_or(&self.name)
    }

    #[allow(dead_code)]
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
async fn list_ollama_models(client: &reqwest::Client, base_url: &str) -> Vec<ModelInfo> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .json::<OllamaTagsResponse>()
            .await
            .map(|r| {
                r.models
                    .into_iter()
                    .map(|e| ModelInfo {
                        name: e.name,
                        size_bytes: e.size,
                    })
                    .collect()
            })
            .unwrap_or_default(),
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
    let mut req = client.get(&url).timeout(std::time::Duration::from_secs(5));
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => resp
            .json::<OpenAiModelsResponse>()
            .await
            .map(|r| {
                r.data
                    .into_iter()
                    .map(|e| ModelInfo {
                        name: e.id,
                        size_bytes: 0,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Query the local backend (Ollama or LM Studio) and return available model names.
/// Non-blocking; returns empty vec on any error.
pub async fn list_local_models(base_url: &str, provider: &str) -> Vec<String> {
    let client = shared_http();
    let infos = match provider {
        "lmstudio" | "lm_studio" | "openai" => {
            list_lmstudio_models(&client, base_url, None).await
        }
        _ => list_ollama_models(&client, base_url).await,
    };
    infos.into_iter().map(|m| m.name).collect()
}

// ── Model capability classification ──────────────────────────────────────────

/// Functional capability of an LLM model.
///
/// A single model may have more than one capability (e.g. a vision-capable
/// coder has both `Vision` and `Code`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModelCapability {
    /// Small/fast model (heuristically < 4 GB or a named mini variant).
    /// Best suited to the router/planner role to keep the main model free.
    Fast,
    /// General conversational capability.  Present on all generative models.
    Chat,
    /// Specialised for code generation, analysis, and editing.
    Code,
    /// Large/powerful model (heuristically ≥ 20 GB or a known large-parameter
    /// name).  Best for deep multi-step reasoning.
    Large,
    /// Multimodal model that accepts image input in addition to text.
    Vision,
    /// Embedding-only model; not suitable for text generation.
    Embedding,
}

/// A model from the backend together with its inferred capabilities.
#[derive(Debug, Clone)]
pub struct ClassifiedModel {
    pub info: ModelInfo,
    pub capabilities: Vec<ModelCapability>,
}

impl ClassifiedModel {
    /// Returns `true` if this model has the given capability.
    pub fn has(&self, cap: &ModelCapability) -> bool {
        self.capabilities.contains(cap)
    }
}

/// Classify a model's capabilities using name patterns and size heuristics.
///
/// Classification order:
/// 1. Embedding-only → [`ModelCapability::Embedding`] only (not generative, stop here).
/// 2. Multimodal/vision names → [`ModelCapability::Vision`].
/// 3. Code-specialised names → [`ModelCapability::Code`].
/// 4. Large by name or size ≥ 20 GB → [`ModelCapability::Large`].
/// 5. Fast by name or size < 4 GB (non-embedding) → [`ModelCapability::Fast`].
/// 6. All generative models → [`ModelCapability::Chat`].
pub fn classify_model_capabilities(model: &ModelInfo) -> Vec<ModelCapability> {
    let name = model.name.to_lowercase();
    let mut caps = Vec::new();

    // ── Embedding-only ────────────────────────────────────────────────────────
    // These models cannot generate text; skip all other capability checks.
    // `bge-` covers the entire BGE family (bge-small, bge-large, bge-m3, etc.)
    // which are embedding/retrieval models not intended for text generation.
    let is_embedding = name.contains("embed")
        || name.contains("nomic")
        || name.contains("all-minilm")
        || name.contains("bge-")
        || name.starts_with("e5-");
    if is_embedding {
        caps.push(ModelCapability::Embedding);
        return caps;
    }

    // ── Vision / multimodal ───────────────────────────────────────────────────
    if name.contains("vision")
        || name.contains("llava")
        || name.contains("bakllava")
        || name.contains("moondream")
        || name.contains("minicpm-v")
        || name.contains("qwen-vl")
        || name.contains("internvl")
        || name.contains("cogvlm")
    {
        caps.push(ModelCapability::Vision);
    }

    // ── Code-specialised ──────────────────────────────────────────────────────
    // Note: `"coder"` is excluded when the name also contains `"decoder"` because
    // "decoder" is a substring of architectures like "decoder-only" and also
    // because "decoder" itself contains the letters c-o-d-e-r as a suffix.
    if name.contains("qwen2.5-coder")
        || name.contains("qwen2.5coder")
        || name.contains("codellama")
        || name.contains("starcoder")
        || name.contains("deepseek-coder")
        || name.contains("devstral")
        || (name.contains("coder") && !name.contains("decoder"))
        || name.contains("code-")
    {
        caps.push(ModelCapability::Code);
    }

    // ── Large model ───────────────────────────────────────────────────────────
    const LARGE_BYTES: u64 = 20 * 1_073_741_824; // 20 GB
    let large_by_name = [
        ":70b", "-70b", ":72b", "-72b", ":65b", "-65b", ":34b", "-34b", ":32b", "-32b", "mixtral",
        "opus",
    ]
    .iter()
    .any(|p| name.contains(p));
    let large_by_size = model.size_bytes > 0 && model.size_bytes >= LARGE_BYTES;
    if large_by_name || large_by_size {
        caps.push(ModelCapability::Large);
    }

    // ── Fast / small ──────────────────────────────────────────────────────────
    const FAST_BYTES: u64 = 4 * 1_073_741_824; // 4 GB
    let fast_by_name = name.contains("tinyllama")
        || name.contains("phi3-mini")
        || name.contains("phi-3-mini")
        || name.contains("phi3:mini")
        || name.contains("qwen:0.5")
        || name.contains("qwen:1.5")
        || name.contains("qwen2:0.5")
        || name.contains("qwen2:1.5")
        || name.contains("smollm")
        || name.contains("gemma:2b")
        || name.contains("tinydolphin");
    let fast_by_size = model.size_bytes > 0 && model.size_bytes < FAST_BYTES;
    if fast_by_name || fast_by_size {
        caps.push(ModelCapability::Fast);
    }

    // ── Chat (all generative models) ─────────────────────────────────────────
    caps.push(ModelCapability::Chat);

    caps
}

// ── Agent fleet ───────────────────────────────────────────────────────────────

/// The set of agents Sirin can run, derived from the models available at startup.
///
/// Built by [`probe_and_build_fleet`] and stored process-wide via
/// [`init_agent_fleet`] / [`shared_fleet`].
#[derive(Debug, Clone)]
pub struct AgentFleet {
    pub backend: LlmBackend,
    pub base_url: String,
    pub api_key: Option<String>,
    /// General-purpose chat model — always present.
    pub chat_model: String,
    /// Fast/small model dedicated to routing and planning.
    /// `None` means the chat model handles routing too.
    pub router_model: Option<String>,
    /// Code-specialised model.
    /// `None` means the chat model handles coding too.
    pub coding_model: Option<String>,
    /// Large/powerful model for deep reasoning.
    /// `None` means the chat model handles large tasks too.
    pub large_model: Option<String>,
    /// Every discovered model with its inferred capabilities.
    pub classified_models: Vec<ClassifiedModel>,
}

#[allow(dead_code)]
impl AgentFleet {
    /// Returns `true` when a dedicated fast router/planner model is available.
    pub fn has_fast_router(&self) -> bool {
        self.router_model.is_some()
    }
    /// Returns `true` when a dedicated code model is available.
    pub fn has_dedicated_coder(&self) -> bool {
        self.coding_model.is_some()
    }
    /// Returns `true` when a dedicated large model is available.
    pub fn has_large_model(&self) -> bool {
        self.large_model.is_some()
    }

    /// Returns `true` when at least one available model has the given capability.
    pub fn has_capability(&self, cap: &ModelCapability) -> bool {
        self.classified_models.iter().any(|m| m.has(cap))
    }

    /// Convert to the [`LlmConfig`] consumed by all LLM call functions.
    pub fn to_llm_config(&self) -> LlmConfig {
        LlmConfig {
            backend: self.backend,
            base_url: self.base_url.clone(),
            model: self.chat_model.clone(),
            api_key: self.api_key.clone(),
            router_model: self.router_model.clone(),
            coding_model: self.coding_model.clone(),
            large_model: self.large_model.clone(),
        }
    }

    /// Write a human-readable fleet summary to stderr.
    pub fn log_summary(&self) {
        eprintln!("[fleet] Agent fleet configured:");
        eprintln!("  chat   → {} (general conversation)", self.chat_model);
        eprintln!(
            "  router → {}",
            self.router_model.as_deref().unwrap_or("(uses chat model)")
        );
        eprintln!(
            "  coder  → {}",
            self.coding_model.as_deref().unwrap_or("(uses chat model)")
        );
        eprintln!(
            "  large  → {}",
            self.large_model.as_deref().unwrap_or("(uses chat model)")
        );

        if self.has_capability(&ModelCapability::Vision) {
            let names: Vec<&str> = self
                .classified_models
                .iter()
                .filter(|m| m.has(&ModelCapability::Vision))
                .map(|m| m.info.name.as_str())
                .collect();
            eprintln!(
                "  vision → {} (multimodal — image input capable)",
                names.join(", ")
            );
        }
        if self.has_capability(&ModelCapability::Embedding) {
            let names: Vec<&str> = self
                .classified_models
                .iter()
                .filter(|m| m.has(&ModelCapability::Embedding))
                .map(|m| m.info.name.as_str())
                .collect();
            eprintln!("  embed  → {} (vector embeddings)", names.join(", "));
        }
    }
}

static SHARED_FLEET: OnceLock<Arc<AgentFleet>> = OnceLock::new();

/// Returns the process-wide agent fleet.
///
/// If [`init_agent_fleet`] has not been called, a minimal fleet built from
/// `LlmConfig::from_env()` is returned (no classified models).
#[allow(dead_code)]
pub(crate) fn shared_fleet() -> Arc<AgentFleet> {
    Arc::clone(SHARED_FLEET.get_or_init(|| {
        let cfg = LlmConfig::from_env();
        Arc::new(AgentFleet {
            backend: cfg.backend,
            base_url: cfg.base_url,
            api_key: cfg.api_key,
            chat_model: cfg.model,
            router_model: cfg.router_model,
            coding_model: cfg.coding_model,
            large_model: cfg.large_model,
            classified_models: Vec::new(),
        })
    }))
}

/// Prime the process-wide fleet singleton.
///
/// Must be called **before** the first call to [`shared_fleet`].  A second call
/// is a no-op because the underlying `OnceLock` is already set.
pub(crate) fn init_agent_fleet(fleet: AgentFleet) {
    let _ = SHARED_FLEET.set(Arc::new(fleet));
}

// ── Capability-aware role selection ──────────────────────────────────────────

/// Pick the best model for a given role from all classified models.
///
/// - [`ModelCapability::Fast`] → smallest by byte size (lowest latency).
/// - [`ModelCapability::Code`] → name-priority order (most specialised first).
/// - [`ModelCapability::Large`] → largest by byte size (most capable).
/// - Other capabilities → first matching model.
///
/// Models that equal `exclude` (the main chat model) are skipped so the router/
/// coding/large slots are always distinct from the main model.
fn best_for_role(
    classified: &[ClassifiedModel],
    cap: &ModelCapability,
    exclude: &str,
) -> Option<String> {
    let candidates: Vec<&ClassifiedModel> = classified
        .iter()
        .filter(|m| m.has(cap) && m.info.name != exclude && m.info.base_name() != exclude)
        .collect();

    if candidates.is_empty() {
        return None;
    }

    match cap {
        ModelCapability::Fast => candidates
            .iter()
            .filter(|m| m.info.size_bytes > 0)
            .min_by_key(|m| m.info.size_bytes)
            .or_else(|| candidates.first())
            .map(|m| m.info.name.clone()),

        ModelCapability::Code => {
            let priority = [
                "qwen2.5-coder",
                "qwen2.5coder",
                "deepseek-coder",
                "devstral",
                "codellama",
                "starcoder",
                "coder",
                "code-",
            ];
            priority
                .iter()
                .find_map(|p| {
                    candidates
                        .iter()
                        .find(|m| m.info.name.to_lowercase().contains(p))
                        .map(|m| m.info.name.clone())
                })
                .or_else(|| candidates.first().map(|m| m.info.name.clone()))
        }

        ModelCapability::Large => candidates
            .iter()
            .filter(|m| m.info.size_bytes > 0)
            .max_by_key(|m| m.info.size_bytes)
            .map(|m| m.info.name.clone()),

        _ => candidates.first().map(|m| m.info.name.clone()),
    }
}

/// Returns `true` when `name` matches any classified model (exact or base-name).
fn is_model_available(name: &str, classified: &[ClassifiedModel]) -> bool {
    let lower = name.to_lowercase();
    let base = lower.split(':').next().unwrap_or(&lower);
    classified.iter().any(|m| {
        let mn = m.info.name.to_lowercase();
        let mb = m.info.base_name().to_lowercase();
        mn == lower || mb == lower || mn == base || mb == base
    })
}

/// Resolve a single role slot from the fleet:
/// - Env var set and model is available → keep.
/// - Env var set but model absent → warn, clear (falls back to chat model).
/// - Env var not set → auto-detect via `best_for_role`.
fn assign_fleet_role(
    role: &str,
    env_value: Option<String>,
    classified: &[ClassifiedModel],
    cap: &ModelCapability,
    chat_model: &str,
) -> Option<String> {
    match env_value {
        Some(ref name) if !name.is_empty() => {
            if is_model_available(name, classified) {
                Some(name.clone())
            } else {
                eprintln!(
                    "[fleet] WARNING: {role} model '{name}' not found — \
                     falling back to chat model '{chat_model}'"
                );
                None
            }
        }
        _ => best_for_role(classified, cap, chat_model),
    }
}

// ── Public probe entry points ─────────────────────────────────────────────────

/// Probe the configured LLM backend at startup, classify every available model
/// by its capabilities, and build an [`AgentFleet`] describing which agents
/// Sirin will run.
///
/// ## Role assignment priority
/// 1. Env var set **and** model confirmed present → use as-is.
/// 2. Env var set **but** model absent → warn + clear (falls back to chat model).
/// 3. Env var not set → auto-detect using [`best_for_role`].
///
/// Non-fatal: on any network error, returns a minimal fleet from env vars only.
pub async fn probe_and_build_fleet(client: &reqwest::Client) -> AgentFleet {
    let baseline = LlmConfig::from_env();

    let raw_models: Vec<ModelInfo> = match baseline.backend {
        LlmBackend::Ollama => list_ollama_models(client, &baseline.base_url).await,
        LlmBackend::LmStudio | LlmBackend::Gemini => {
            list_lmstudio_models(client, &baseline.base_url, baseline.api_key.as_deref()).await
        }
    };

    if raw_models.is_empty() {
        eprintln!(
            "[fleet] {} at '{}' returned no models — using env-only config",
            baseline.backend_name(),
            baseline.base_url,
        );
        return AgentFleet {
            backend: baseline.backend,
            base_url: baseline.base_url,
            api_key: baseline.api_key,
            chat_model: baseline.model,
            router_model: baseline.router_model,
            coding_model: baseline.coding_model,
            large_model: baseline.large_model,
            classified_models: Vec::new(),
        };
    }

    // Classify every model.
    let classified: Vec<ClassifiedModel> = raw_models
        .into_iter()
        .map(|info| {
            let capabilities = classify_model_capabilities(&info);
            ClassifiedModel { info, capabilities }
        })
        .collect();

    // Log the discovered catalogue.
    eprintln!(
        "[fleet] {} model(s) found on {} ({})",
        classified.len(),
        baseline.backend_name(),
        baseline.base_url,
    );

    // Validate / pick the main chat model.
    let chat_model = if is_model_available(&baseline.model, &classified) {
        baseline.model.clone()
    } else {
        let fallback = classified
            .iter()
            .find(|m| m.has(&ModelCapability::Chat))
            .or_else(|| classified.first())
            .map(|m| m.info.name.clone())
            .unwrap_or(baseline.model.clone());
        if fallback != baseline.model {
            eprintln!(
                "[fleet] WARNING: main model '{}' not found — \
                 using first available Chat model '{fallback}'",
                baseline.model
            );
        }
        fallback
    };

    // Assign router / coding / large roles.
    let router_model = assign_fleet_role(
        "router",
        baseline.router_model,
        &classified,
        &ModelCapability::Fast,
        &chat_model,
    );
    let coding_model = assign_fleet_role(
        "coding",
        baseline.coding_model,
        &classified,
        &ModelCapability::Code,
        &chat_model,
    );
    let large_model = assign_fleet_role(
        "large",
        baseline.large_model,
        &classified,
        &ModelCapability::Large,
        &chat_model,
    );

    AgentFleet {
        backend: baseline.backend,
        base_url: baseline.base_url,
        api_key: baseline.api_key,
        chat_model,
        router_model,
        coding_model,
        large_model,
        classified_models: classified,
    }
}

/// Probe the backend and return a plain [`LlmConfig`].
///
/// This is a convenience wrapper around [`probe_and_build_fleet`] for callers
/// that only need the config and not the full fleet.
#[allow(dead_code)]
pub async fn probe_and_configure(client: &reqwest::Client) -> LlmConfig {
    probe_and_build_fleet(client).await.to_llm_config()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ollama_cfg() -> LlmConfig {
        LlmConfig {
            backend: LlmBackend::Ollama,
            base_url: "http://localhost:11434".to_string(),
            model: "llama3.2".to_string(),
            api_key: None,
            coding_model: None,
            router_model: None,
            large_model: None,
        }
    }

    // ── effective_* fallback chain ────────────────────────────────────────────

    #[test]
    fn effective_models_fall_back_to_main_model() {
        let cfg = ollama_cfg();
        assert_eq!(cfg.effective_coding_model(), "llama3.2");
        assert_eq!(cfg.effective_router_model(), "llama3.2");
        assert_eq!(cfg.effective_large_model(), "llama3.2");
    }

    #[test]
    fn effective_models_prefer_dedicated_when_set() {
        let cfg = LlmConfig {
            coding_model: Some("qwen2.5-coder".to_string()),
            router_model: Some("phi3-mini".to_string()),
            large_model: Some("llama3:70b".to_string()),
            ..ollama_cfg()
        };
        assert_eq!(cfg.effective_coding_model(), "qwen2.5-coder");
        assert_eq!(cfg.effective_router_model(), "phi3-mini");
        assert_eq!(cfg.effective_large_model(), "llama3:70b");
    }

    #[test]
    fn effective_partial_override_falls_back_correctly() {
        // Only coding_model set; router and large fall back to main.
        let cfg = LlmConfig {
            coding_model: Some("qwen-coder:7b".to_string()),
            ..ollama_cfg()
        };
        assert_eq!(cfg.effective_coding_model(), "qwen-coder:7b");
        assert_eq!(
            cfg.effective_router_model(),
            "llama3.2",
            "router should fall back"
        );
        assert_eq!(
            cfg.effective_large_model(),
            "llama3.2",
            "large should fall back"
        );
    }

    #[test]
    fn backend_name_returns_correct_string() {
        assert_eq!(ollama_cfg().backend_name(), "ollama");
        let lm = LlmConfig {
            backend: LlmBackend::LmStudio,
            ..ollama_cfg()
        };
        assert_eq!(lm.backend_name(), "lmstudio");
    }

    #[test]
    fn task_log_summary_lists_effective_models() {
        let cfg = LlmConfig {
            coding_model: Some("qwen2.5-coder".to_string()),
            router_model: Some("phi3-mini".to_string()),
            large_model: Some("llama3:70b".to_string()),
            ..ollama_cfg()
        };

        let summary = cfg.task_log_summary();
        assert!(
            summary.contains("backend=ollama"),
            "unexpected summary: {summary}"
        );
        assert!(
            summary.contains("chat=ollama/llama3.2"),
            "unexpected summary: {summary}"
        );
        assert!(
            summary.contains("router=") && summary.contains("phi3-mini"),
            "unexpected summary: {summary}"
        );
        assert!(
            summary.contains("coding=ollama/qwen2.5-coder"),
            "unexpected summary: {summary}"
        );
        assert!(
            summary.contains("large=ollama/llama3:70b"),
            "unexpected summary: {summary}"
        );
    }

    // ── LlmConfig::from_env smoke test (read-only, no mutation) ──────────────

    #[test]
    fn from_env_succeeds_without_panicking() {
        // Should not panic regardless of what env vars are set.
        let cfg = LlmConfig::from_env();
        assert!(
            !cfg.model.is_empty(),
            "model must not be empty after from_env"
        );
        assert!(
            !cfg.base_url.is_empty(),
            "base_url must not be empty after from_env"
        );
    }

    // ── MessageRole ───────────────────────────────────────────────────────────

    #[test]
    fn message_role_as_str_matches_openai_convention() {
        assert_eq!(MessageRole::System.as_str(), "system");
        assert_eq!(MessageRole::User.as_str(), "user");
        assert_eq!(MessageRole::Assistant.as_str(), "assistant");
    }

    #[test]
    fn llm_message_constructors_set_correct_roles() {
        let sys = LlmMessage::system("you are an assistant");
        let usr = LlmMessage::user("hello");
        let ast = LlmMessage::assistant("hi there");
        assert_eq!(sys.role, MessageRole::System);
        assert_eq!(usr.role, MessageRole::User);
        assert_eq!(ast.role, MessageRole::Assistant);
        assert_eq!(usr.content, "hello");
    }
}
