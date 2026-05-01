//! Shared LLM provider abstraction — Ollama and OpenAI-compatible (LM Studio,
//! Gemini, Anthropic) backends.
//!
//! ## Concurrency
//! Four `OnceLock` singletons — [`shared_http`], [`shared_llm`],
//! [`shared_router_llm`], [`shared_large_llm`] — are initialised on first
//! access and never mutated after.  Reqwest's `Client` is internally
//! thread-safe and designed for concurrent reuse.  `LlmConfig` is cloned
//! into the three derived singletons; changing env vars or `config/llm.yaml`
//! after startup requires a process restart to take effect.
//!
//! ## Contents
//! - Core types ([`LlmConfig`], [`LlmBackend`], [`MessageRole`], [`LlmMessage`]).
//! - Process-wide singletons ([`shared_http`], [`shared_llm`], [`shared_router_llm`],
//!   [`shared_large_llm`]).
//! - UI-editable config ([`LlmUiConfig`] — persisted to `config/llm.yaml`).
//! - Public `call_prompt*` and `call_prompt_stream` entry points.
//!
//! Submodules:
//! - [`backends`] — HTTP wire types + `call_ollama` / `call_openai` / streaming.
//! - [`probe`] — backend probing, capability classification, and [`probe::AgentFleet`].
//!
//! ## Environment variables
//!
//! | Variable            | Default                        | Description                     |
//! |---------------------|--------------------------------|---------------------------------|
//! | `LLM_PROVIDER`      | `ollama`                       | `ollama`, `lmstudio`/`openai`, `gemini`, `anthropic` |
//! | `OLLAMA_BASE_URL`   | `http://localhost:11434`       | Ollama server address           |
//! | `OLLAMA_MODEL`      | `llama3.2`                     | Main model name                 |
//! | `LM_STUDIO_BASE_URL`| `http://localhost:1234/v1`     | LM Studio / OpenAI endpoint     |
//! | `LM_STUDIO_MODEL`   | `llama3.2`                     | Main model name                 |
//! | `LM_STUDIO_API_KEY` | *(empty)*                      | Optional Bearer token           |
//! | `ROUTER_MODEL`      | *(falls back to main model)*   | Small model for Router/Planner; kept resident in Ollama via `keep_alive=-1` |
//! | `CODING_MODEL`      | *(falls back to main model)*   | Dedicated model for CodingAgent |
//! | `LARGE_MODEL`       | *(falls back to main model)*   | Large model for deep reasoning  |
//! | `LLM_FALLBACK_BASE_URL` | *(none)*                   | OpenAI-compat URL for 429 fallback (e.g. `https://api.deepseek.com/v1`) |
//! | `LLM_FALLBACK_API_KEY`  | *(none)*                   | API key for fallback provider   |
//! | `LLM_FALLBACK_MODEL`    | *(none)*                   | Model name for fallback (e.g. `deepseek-chat`) |

mod backends;
mod probe;

pub use probe::probe_and_build_fleet;
#[allow(unused_imports)]
pub(crate) use probe::{init_agent_fleet, shared_fleet, ModelCapability};

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use backends::{call_ollama, call_openai, call_openai_messages, stream_ollama, stream_openai, OpenAiMessage};

const OLLAMA_BASE_URL: &str = "http://localhost:11434";
const LM_STUDIO_BASE_URL:  &str = "http://localhost:1234/v1";
const GEMINI_BASE_URL:     &str = "https://generativelanguage.googleapis.com/v1beta/openai";
const ANTHROPIC_BASE_URL:  &str = "https://api.anthropic.com/v1";
const DEFAULT_MODEL:       &str = "llama3.2";
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.0-flash";
const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-4-6";

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

/// Returns the process-wide **fallback** LLM config, initialised from
/// `LLM_FALLBACK_{BASE_URL,API_KEY,MODEL}` environment variables.
///
/// Returns `None` when no fallback is configured (variables absent or empty).
/// Used by [`call_coding_prompt`] and [`call_prompt`] to switch providers
/// instantly when the primary returns a 429 rate-limit error, avoiding the
/// previous 30s+60s+120s retry wait.
pub(crate) fn fallback_llm() -> Option<Arc<LlmConfig>> {
    static FALLBACK: OnceLock<Option<Arc<LlmConfig>>> = OnceLock::new();
    FALLBACK
        .get_or_init(|| {
            let base_url = std::env::var("LLM_FALLBACK_BASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())?;
            let model = std::env::var("LLM_FALLBACK_MODEL")
                .ok()
                .filter(|v| !v.trim().is_empty())?;
            let api_key = std::env::var("LLM_FALLBACK_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let cfg = LlmConfig {
                backend: LlmBackend::LmStudio, // OpenAI-compat covers DeepSeek, OpenRouter, etc.
                base_url,
                model: model.clone(),
                api_key,
                coding_model: None,
                router_model: None,
                large_model: None,
            };
            crate::sirin_log!("[llm] fallback configured: model={}", model);
            Some(Arc::new(cfg))
        })
        .clone()
}

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
    fn path() -> std::path::PathBuf {
        crate::platform::config_path("llm.yaml")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(content) => serde_yaml::from_str(&content).unwrap_or_else(|e| {
                crate::sirin_log!("[llm] Failed to parse llm.yaml: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let p = Self::path();
        if let Some(parent) = p.parent() { let _ = std::fs::create_dir_all(parent); }
        let content = serde_yaml::to_string(self)?;
        std::fs::write(&p, content)?;
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
    /// Anthropic Claude via the OpenAI-compatible endpoint at
    /// `https://api.anthropic.com/v1`.
    /// Set `LLM_PROVIDER=anthropic` and `ANTHROPIC_API_KEY=<key>`.
    Anthropic,
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
            "anthropic" | "claude" => Self {
                backend: LlmBackend::Anthropic,
                base_url: std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| ANTHROPIC_BASE_URL.to_string()),
                model: std::env::var("ANTHROPIC_MODEL")
                    .unwrap_or_else(|_| DEFAULT_CLAUDE_MODEL.to_string()),
                api_key: std::env::var("ANTHROPIC_API_KEY")
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
                "anthropic" | "claude" => self.backend = LlmBackend::Anthropic,
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
            LlmBackend::Ollama     => "ollama",
            LlmBackend::LmStudio   => "lmstudio",
            LlmBackend::Gemini     => "gemini",
            LlmBackend::Anthropic  => "anthropic",
        }
    }

    /// Returns true when this config points to a cloud/remote backend.
    pub fn is_remote(&self) -> bool {
        matches!(self.backend, LlmBackend::Gemini | LlmBackend::Anthropic)
    }

    /// Build a minimal `LlmConfig` for a per-agent override.
    ///
    /// Used by `AgentConfig::resolve_llm_override` to construct an on-the-fly
    /// config for agents that use a different provider (e.g. Anthropic Claude).
    ///
    /// `backend` is one of `"anthropic"`, `"lmstudio"`, `"gemini"`, `"ollama"`.
    /// If unrecognised, falls back to Ollama.
    pub fn for_override(backend: &str, model: &str, api_key: Option<String>) -> Self {
        let (b, base_url) = match backend.to_lowercase().as_str() {
            "anthropic" | "claude" => (LlmBackend::Anthropic, ANTHROPIC_BASE_URL.to_string()),
            "lmstudio" | "openai"  => (LlmBackend::LmStudio,  LM_STUDIO_BASE_URL.to_string()),
            "gemini" | "google"    => (LlmBackend::Gemini,     GEMINI_BASE_URL.to_string()),
            _                      => (LlmBackend::Ollama,     OLLAMA_BASE_URL.to_string()),
        };
        Self {
            backend: b,
            base_url,
            model: model.to_string(),
            api_key,
            coding_model: None,
            router_model: None,
            large_model:  None,
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

// ── Vision specialist LLM config ─────────────────────────────────────────────

/// Build an `LlmConfig` for vision-only calls (e.g. `screenshot_analyze`).
///
/// Reads four env vars:
/// - `LLM_VISION_BACKEND`  — `ollama` | `lmstudio` | `gemini` | `anthropic`
/// - `LLM_VISION_BASE_URL` — endpoint base URL
/// - `LLM_VISION_MODEL`    — model name (e.g. `qwen/qwen2.5-vl-7b-instruct`)
/// - `LLM_VISION_API_KEY`  — Bearer token
///
/// Returns `Some(LlmConfig)` only when **all four** are present and non-empty;
/// returns `None` otherwise so callers fall back to the main LLM without change.
pub fn vision_llm_config() -> Option<LlmConfig> {
    let backend_str = std::env::var("LLM_VISION_BACKEND")
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    let base_url = std::env::var("LLM_VISION_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    let model = std::env::var("LLM_VISION_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty())?;

    // Warn explicitly when base_url is set but api_key is missing, so the user
    // gets a clear signal that the vision specialist will be silently disabled.
    let raw_api_key = std::env::var("LLM_VISION_API_KEY").unwrap_or_default();
    if raw_api_key.trim().is_empty() {
        tracing::warn!(
            target: "sirin",
            "[llm] LLM_VISION_BASE_URL is set ({}) but LLM_VISION_API_KEY is empty — \
             vision specialist DISABLED. All screenshot_analyze calls will use the main LLM.",
            base_url
        );
        return None;
    }
    let api_key = raw_api_key;

    let backend = match backend_str.to_lowercase().as_str() {
        "lmstudio" | "lm_studio" | "openai" => LlmBackend::LmStudio,
        "gemini" | "google" => LlmBackend::Gemini,
        "anthropic" | "claude" => LlmBackend::Anthropic,
        _ => LlmBackend::Ollama,
    };

    Some(LlmConfig {
        backend,
        base_url,
        model,
        api_key: Some(api_key),
        coding_model: None,
        router_model: None,
        large_model: None,
    })
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

// ── Public API ────────────────────────────────────────────────────────────────

/// Convenience wrapper: uses the shared LLM config and the process-wide HTTP client.
/// Intended for one-off UI calls that don't have an existing client/config in scope.
/// Returns `true` when the error string indicates a 429 rate-limit response
/// from the backends layer.  Used by call sites to decide whether to try
/// the configured fallback provider.
pub(crate) fn is_rate_limit_err(e: &(dyn std::error::Error + Send + Sync)) -> bool {
    let s = e.to_string();
    s.contains("429") || s.contains("Too Many Requests") || s.contains("rate-limited: max retries")
}

/// Inner helper: call an OpenAI-compatible endpoint with an explicit optional
/// fallback config.  On 429 from primary, switches to fallback immediately.
///
/// Exposed as `pub(crate)` so tests can inject arbitrary mock configs without
/// going through the `OnceLock`-backed `fallback_llm()` singleton.
pub(crate) async fn call_openai_with_fallback(
    client: &reqwest::Client,
    primary_url: &str,
    primary_model: &str,
    primary_key: Option<&str>,
    fallback: Option<&LlmConfig>,
    prompt: String,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let result = call_openai(client, primary_url, primary_model, primary_key, prompt.clone()).await;
    if let Err(ref e) = result {
        if is_rate_limit_err(e.as_ref()) {
            if let Some(fb) = fallback {
                crate::sirin_log!(
                    "[llm] 429 on primary ({}) — falling back to {}",
                    primary_model,
                    fb.model
                );
                return call_openai(
                    client,
                    &fb.base_url,
                    &fb.model,
                    fb.api_key.as_deref(),
                    prompt,
                )
                .await;
            }
        }
    }
    result
}

pub async fn call_llm_simple(
    prompt: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let client = shared_http();
    let llm = shared_llm();
    call_prompt(&client, &llm, prompt).await
}

/// Send `prompt` to the configured LLM backend and return the trimmed response.
///
/// On 429 rate-limit from the primary provider, automatically retries once
/// using the fallback LLM (configured via `LLM_FALLBACK_*` env vars).
/// If no fallback is set, or the fallback also fails, returns the original error.
pub async fn call_prompt(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: impl Into<String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = prompt.into();
    match llm.backend {
        LlmBackend::Ollama => {
            call_ollama(client, &llm.base_url, &llm.model, prompt, None).await
        }
        LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic => {
            call_openai_with_fallback(
                client, &llm.base_url, &llm.model, llm.api_key.as_deref(),
                fallback_llm().as_deref(), prompt,
            ).await
        }
    }
}

/// Like [`call_prompt`] but uses the coding-specific model when configured.
///
/// On 429 rate-limit from the primary provider, automatically retries once
/// using the fallback LLM (configured via `LLM_FALLBACK_*` env vars).
/// This is the primary call path for the test runner — instant fallback
/// prevents the 429 retry wait (35 s) from cascading into test timeouts.
pub async fn call_coding_prompt(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: impl Into<String>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = prompt.into();
    let model = llm.effective_coding_model();
    match llm.backend {
        LlmBackend::Ollama => {
            call_ollama(client, &llm.base_url, model, prompt, None).await
        }
        LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic => {
            call_openai_with_fallback(
                client, &llm.base_url, model, llm.api_key.as_deref(),
                fallback_llm().as_deref(), prompt,
            ).await
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
        LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic => {
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
        LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic => {
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
        LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic => {
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
        LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic => {
            let openai_msgs: Vec<OpenAiMessage> = messages
                .iter()
                .map(|m| OpenAiMessage::text(m.role.as_str(), &m.content))
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

// ── Vision / multimodal ──────────────────────────────────────────────────────

/// Send a prompt with an image to the LLM (vision-capable models only).
///
/// `image_base64` is raw base64-encoded PNG/JPEG data (no data: prefix).
/// Works with Gemini, GPT-4o, Claude — NOT with Ollama text-only models.
#[allow(dead_code)]
pub async fn call_vision(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: &str,
    image_base64: &str,
    mime: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    match llm.backend {
        LlmBackend::Ollama => {
            // Ollama vision models (llava, etc.) use the `images` field
            let body = serde_json::json!({
                "model": llm.model,
                "prompt": prompt,
                "images": [image_base64],
                "stream": false,
            });
            let url = format!("{}/api/generate", llm.base_url.trim_end_matches('/'));
            let resp: serde_json::Value = client.post(&url).json(&body).send().await?.json().await?;
            Ok(resp["response"].as_str().unwrap_or("").trim().to_string())
        }
        LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic => {
            let msg = OpenAiMessage::with_image("user", prompt, image_base64, mime);
            call_openai_messages(
                client,
                &llm.base_url,
                &llm.model,
                llm.api_key.as_deref(),
                vec![msg],
            ).await
        }
    }
}

/// Convenience: screenshot current browser page and ask LLM to analyze it.
#[allow(dead_code)]
pub async fn analyze_screenshot(
    client: &reqwest::Client,
    llm: &LlmConfig,
    prompt: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let png = tokio::task::spawn_blocking(crate::browser::screenshot)
        .await
        .map_err(|e| format!("spawn: {e}"))??;
    let b64 = base64_encode_bytes(&png);
    call_vision(client, llm, prompt, &b64, "image/png").await
}

pub(crate) fn base64_encode_bytes(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 { out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char); }
        else { out.push('='); }
        if chunk.len() > 2 { out.push(CHARS[(triple & 0x3F) as usize] as char); }
        else { out.push('='); }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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
        assert!(summary.contains("backend=ollama"), "unexpected summary: {summary}");
        assert!(summary.contains("chat=ollama/llama3.2"), "unexpected summary: {summary}");
        assert!(summary.contains("router=") && summary.contains("phi3-mini"), "unexpected summary: {summary}");
        assert!(summary.contains("coding=ollama/qwen2.5-coder"), "unexpected summary: {summary}");
        assert!(summary.contains("large=ollama/llama3:70b"), "unexpected summary: {summary}");
    }

    #[test]
    fn from_env_succeeds_without_panicking() {
        let cfg = LlmConfig::from_env();
        assert!(!cfg.model.is_empty(), "model must not be empty after from_env");
        assert!(!cfg.base_url.is_empty(), "base_url must not be empty after from_env");
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

    // ── vision_llm_config ─────────────────────────────────────────────────────

    #[test]
    fn vision_llm_config_reads_env_vars() {
        // Scope env-var changes so they don't leak into other tests.
        // (Tests within the same binary share the process environment.)
        std::env::set_var("LLM_VISION_BACKEND",  "lmstudio");
        std::env::set_var("LLM_VISION_BASE_URL", "https://openrouter.ai/api/v1");
        std::env::set_var("LLM_VISION_MODEL",    "qwen/qwen2.5-vl-7b-instruct");
        std::env::set_var("LLM_VISION_API_KEY",  "sk-test-key");

        let cfg = super::vision_llm_config();

        // Cleanup before any assert (so a failing assert doesn't leave vars set).
        std::env::remove_var("LLM_VISION_BACKEND");
        std::env::remove_var("LLM_VISION_BASE_URL");
        std::env::remove_var("LLM_VISION_MODEL");
        std::env::remove_var("LLM_VISION_API_KEY");

        let cfg = cfg.expect("should return Some when all four vars are set");
        assert_eq!(cfg.backend, LlmBackend::LmStudio);
        assert_eq!(cfg.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(cfg.model, "qwen/qwen2.5-vl-7b-instruct");
        assert_eq!(cfg.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn vision_llm_config_returns_none_if_incomplete() {
        // Only set three of the four required vars — should return None.
        std::env::set_var("LLM_VISION_BACKEND",  "gemini");
        std::env::set_var("LLM_VISION_BASE_URL", "https://example.com");
        std::env::set_var("LLM_VISION_MODEL",    "gemini-vision");
        // LLM_VISION_API_KEY intentionally omitted.
        std::env::remove_var("LLM_VISION_API_KEY");

        let result = super::vision_llm_config();

        std::env::remove_var("LLM_VISION_BACKEND");
        std::env::remove_var("LLM_VISION_BASE_URL");
        std::env::remove_var("LLM_VISION_MODEL");

        assert!(result.is_none(), "should return None when API key is missing");
    }

    #[test]
    fn vision_llm_config_warns_when_base_url_set_but_key_empty() {
        // Arrange: base_url + backend + model are set, but api_key is empty string.
        std::env::set_var("LLM_VISION_BACKEND",  "lmstudio");
        std::env::set_var("LLM_VISION_BASE_URL", "https://openrouter.ai/api/v1");
        std::env::set_var("LLM_VISION_MODEL",    "qwen/qwen2.5-vl-7b-instruct");
        std::env::set_var("LLM_VISION_API_KEY",  "");

        let result = super::vision_llm_config();

        // Cleanup before assertions so env doesn't leak on failure.
        std::env::remove_var("LLM_VISION_BACKEND");
        std::env::remove_var("LLM_VISION_BASE_URL");
        std::env::remove_var("LLM_VISION_MODEL");
        std::env::remove_var("LLM_VISION_API_KEY");

        // The function must return None (vision specialist disabled) when
        // LLM_VISION_API_KEY is set but empty — the warning is emitted inside
        // the function; we just assert the observable return value here.
        assert!(
            result.is_none(),
            "should return None and warn when base_url is set but api_key is empty"
        );
    }

    // ── 429 fallback chain ────────────────────────────────────────────────────

    #[test]
    fn is_rate_limit_err_detects_various_429_formats() {
        // Standard HTTP error string from reqwest
        let e: Box<dyn std::error::Error + Send + Sync> =
            "HTTP status client error (429 Too Many Requests)".into();
        assert!(super::is_rate_limit_err(e.as_ref()), "should detect HTTP 429 status");

        // Our custom message from backends.rs after max retries
        let e: Box<dyn std::error::Error + Send + Sync> =
            "429 rate-limited: max retries exceeded for model=models/gemini-2.5-flash".into();
        assert!(super::is_rate_limit_err(e.as_ref()), "should detect custom rate-limit message");

        // Plain 429 substring
        let e: Box<dyn std::error::Error + Send + Sync> = "429".into();
        assert!(super::is_rate_limit_err(e.as_ref()), "should detect bare 429");

        // OpenAI error body
        let e: Box<dyn std::error::Error + Send + Sync> =
            "error: Too Many Requests, please slow down".into();
        assert!(super::is_rate_limit_err(e.as_ref()), "should detect Too Many Requests");

        // Non-429 errors must NOT match
        let e: Box<dyn std::error::Error + Send + Sync> = "connection refused".into();
        assert!(!super::is_rate_limit_err(e.as_ref()), "connection refused is not 429");

        let e: Box<dyn std::error::Error + Send + Sync> =
            "HTTP status client error (401 Unauthorized)".into();
        assert!(!super::is_rate_limit_err(e.as_ref()), "401 is not 429");

        let e: Box<dyn std::error::Error + Send + Sync> = "timeout elapsed".into();
        assert!(!super::is_rate_limit_err(e.as_ref()), "timeout is not 429");
    }

    // ── call_openai_with_fallback — mock server integration test ──────────────
    //
    // Spawns two minimal HTTP servers:
    //   - primary_server: always returns 429 with the custom error body
    //   - fallback_server: returns 200 with a valid OpenAI-compat JSON response
    //
    // Calls `call_openai_with_fallback` with explicit primary+fallback configs
    // (bypasses the OnceLock singleton so no real API keys are needed).
    // Asserts that:
    //   1. primary_server receives exactly one request
    //   2. fallback_server receives exactly one request
    //   3. the returned text is "pong" (from fallback response)

    // ── mock server helpers ───────────────────────────────────────────────────

    /// Spawn a mock HTTP server that returns `status` for all connections.
    /// Uses `Retry-After: 0` so `call_openai_messages` retries without delay.
    /// Returns (addr, hit_counter).
    async fn spawn_mock_server(
        status: u16,
        body: &'static str,
    ) -> (std::net::SocketAddr, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = Arc::clone(&hits);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reason = if status == 429 { "Too Many Requests" } else { "OK" };

        tokio::spawn(async move {
            // Accept up to 10 connections so retry loops are served correctly.
            for _ in 0..10 {
                match listener.accept().await {
                    Ok((mut stream, _)) => {
                        let mut buf = vec![0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // Retry-After: 0 → no sleep between retries in backends.rs
                        let resp = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {len}\r\n\
                             Retry-After: 0\r\n\
                             Connection: close\r\n\r\n{body}",
                            len = body.len(),
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    }
                    Err(_) => break,
                }
            }
        });
        (addr, hits)
    }

    #[tokio::test]
    async fn fallback_chain_switches_to_fallback_on_429() {
        // primary: always 429 with Retry-After: 0 (no sleep between retries)
        let body_429 = r#"{"error":{"message":"rate limit","code":429}}"#;
        let (primary_addr, primary_hits) = spawn_mock_server(429, body_429).await;

        // fallback: returns valid OpenAI-compat JSON with "pong"
        let body_ok = r#"{"choices":[{"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}]}"#;
        let (fallback_addr, fallback_hits) = spawn_mock_server(200, body_ok).await;

        let client = reqwest::Client::new();
        let fallback_cfg = LlmConfig {
            backend: LlmBackend::LmStudio,
            base_url: format!("http://{fallback_addr}"),
            model: "fallback-model".to_string(),
            api_key: None,
            coding_model: None,
            router_model: None,
            large_model: None,
        };

        let result = super::call_openai_with_fallback(
            &client,
            &format!("http://{primary_addr}"),
            "primary-model",
            None,
            Some(&fallback_cfg),
            "say: pong".to_string(),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(result.is_ok(), "call should succeed via fallback; got: {result:?}");
        assert_eq!(result.unwrap(), "pong", "fallback response should be 'pong'");
        // primary hit 4× (initial + 3 retries, check fires on attempt 3):
        //   attempt 0 → 429 (sleep 0s, rate_limit_attempt=1)
        //   attempt 1 → 429 (sleep 0s, rate_limit_attempt=2)
        //   attempt 2 → 429 (sleep 0s, rate_limit_attempt=3)
        //   attempt 3 → 429, rate_limit_attempt >= 3 → return Err
        assert_eq!(primary_hits.load(std::sync::atomic::Ordering::SeqCst), 4,
            "primary must receive initial + 3 retry attempts (4 total)");
        assert_eq!(fallback_hits.load(std::sync::atomic::Ordering::SeqCst), 1,
            "fallback must be hit exactly once");
    }

    #[tokio::test]
    async fn no_fallback_configured_returns_429_error() {
        // primary: always 429 (all 3 retry attempts served)
        let body_429 = r#"{"error":{"message":"rate limit","code":429}}"#;
        let (addr, _hits) = spawn_mock_server(429, body_429).await;

        let client = reqwest::Client::new();
        let result = super::call_openai_with_fallback(
            &client,
            &format!("http://{addr}"),
            "primary-model",
            None,
            None, // no fallback configured
            "hello".to_string(),
        )
        .await;

        assert!(result.is_err(), "should return Err when no fallback is configured");
        let err_str = result.unwrap_err().to_string();
        let err_box: Box<dyn std::error::Error + Send + Sync> = err_str.clone().into();
        assert!(
            super::is_rate_limit_err(&err_box),
            "error should indicate rate limit; got: {err_str}"
        );
    }
}
