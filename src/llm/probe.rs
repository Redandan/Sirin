//! Backend probing, model classification, and agent-fleet construction.
//!
//! At startup, [`probe_and_build_fleet`] queries the configured backend
//! (Ollama / LM Studio / Gemini / Anthropic), classifies every discovered
//! model by capability (Fast/Chat/Code/Large/Vision/Embedding), and assigns
//! the best candidate to each role slot (chat / router / coding / large).
//!
//! If the configured backend is unreachable, the probe falls back to local
//! Ollama → LM Studio so Sirin can still launch without a pre-configured
//! `.env`.

use std::sync::{Arc, OnceLock};

use serde::Deserialize;

use super::{shared_http, LlmBackend, LlmConfig};

// ── Model descriptor ─────────────────────────────────────────────────────────

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

// ── Backend response types ───────────────────────────────────────────────────

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

#[derive(Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelEntry>,
}

#[derive(Deserialize)]
struct OpenAiModelEntry {
    id: String,
}

// ── Model listing ────────────────────────────────────────────────────────────

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
        || name.contains("qwen2.5-vl")
        || name.contains("qwen2-vl")
        || name.contains("internvl")
        || name.contains("cogvlm")
        || name.contains("gemma-3")   // Gemma 3+ are multimodal
        || name.contains("gemma-4")
        || name.contains("gemma3")
        || name.contains("gemma4")
        || name.contains("phi-3.5-vision")
        || name.contains("phi-4")
    {
        caps.push(ModelCapability::Vision);
    }

    // ── Code-specialised ──────────────────────────────────────────────────────
    // "decoder" is excluded because it contains "coder" as a suffix.
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
    // `>= LARGE_BYTES` (20 GB) already implies `> 0`, so no size-known guard needed here.
    let large_by_size = model.size_bytes >= LARGE_BYTES;
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

// ── Agent fleet ──────────────────────────────────────────────────────────────

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

// ── Public probe entry points ────────────────────────────────────────────────

/// Candidate local LLM services tried in order when the configured backend
/// returns no models.  Each entry is `(backend, base_url)`.
const LOCAL_FALLBACK_BACKENDS: &[(LlmBackend, &str)] = &[
    (LlmBackend::Ollama,   "http://localhost:11434"),
    (LlmBackend::LmStudio, "http://localhost:1234/v1"),
];

/// Try every entry in [`LOCAL_FALLBACK_BACKENDS`] and return the first one
/// that responds with at least one model.  Returns `None` when all fail.
async fn auto_probe_local_backends(
    client: &reqwest::Client,
) -> Option<(LlmBackend, String, Vec<ModelInfo>)> {
    for &(backend, url) in LOCAL_FALLBACK_BACKENDS {
        let models = match backend {
            LlmBackend::Ollama => list_ollama_models(client, url).await,
            _ => list_lmstudio_models(client, url, None).await,
        };
        if !models.is_empty() {
            eprintln!(
                "[fleet] auto-detected {} at '{}' ({} model(s))",
                match backend {
                    LlmBackend::Ollama   => "Ollama",
                    LlmBackend::LmStudio => "LM Studio",
                    _                    => "unknown",
                },
                url,
                models.len(),
            );
            return Some((backend, url.to_string(), models));
        }
    }
    None
}

/// Probe the configured LLM backend at startup, classify every available model
/// by its capabilities, and build an [`AgentFleet`] describing which agents
/// Sirin will run.
///
/// ## Role assignment priority
/// 1. Env var set **and** model confirmed present → use as-is.
/// 2. Env var set **but** model absent → warn + clear (falls back to chat model).
/// 3. Env var not set → auto-detect using `best_for_role`.
///
/// ## Auto-detection fallback
/// If the configured backend returns no models (service not running or not
/// configured), the function automatically probes Ollama → LM Studio in order
/// and uses the first responding service.  This lets Sirin start correctly
/// without a pre-configured `.env`.
///
/// Non-fatal: if no local LLM service is found, returns a minimal fleet so
/// the GUI still launches.
pub async fn probe_and_build_fleet(client: &reqwest::Client) -> AgentFleet {
    let baseline = LlmConfig::from_env();

    // Try the configured backend first.
    let mut raw_models: Vec<ModelInfo> = match baseline.backend {
        LlmBackend::Ollama => list_ollama_models(client, &baseline.base_url).await,
        LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic => {
            list_lmstudio_models(client, &baseline.base_url, baseline.api_key.as_deref()).await
        }
    };

    // If the configured backend is unreachable or empty, auto-probe local services.
    let (active_backend, active_url, active_api_key) =
        if raw_models.is_empty() && matches!(baseline.backend, LlmBackend::Ollama | LlmBackend::LmStudio) {
            eprintln!(
                "[fleet] {} at '{}' returned no models — probing local services…",
                baseline.backend_name(),
                baseline.base_url,
            );
            if let Some((b, url, models)) = auto_probe_local_backends(client).await {
                raw_models = models;
                (b, url, None)
            } else {
                eprintln!("[fleet] No local LLM service found. Start Ollama or LM Studio to enable AI.");
                return AgentFleet {
                    backend:           baseline.backend,
                    base_url:          baseline.base_url,
                    api_key:           baseline.api_key,
                    chat_model:        baseline.model,
                    router_model:      baseline.router_model,
                    coding_model:      baseline.coding_model,
                    large_model:       baseline.large_model,
                    classified_models: Vec::new(),
                };
            }
        } else {
            (baseline.backend, baseline.base_url.clone(), baseline.api_key.clone())
        };

    if raw_models.is_empty() {
        eprintln!(
            "[fleet] {} at '{}' returned no models — using env-only config",
            baseline.backend_name(),
            active_url,
        );
        return AgentFleet {
            backend:           active_backend,
            base_url:          active_url,
            api_key:           active_api_key,
            chat_model:        baseline.model,
            router_model:      baseline.router_model,
            coding_model:      baseline.coding_model,
            large_model:       baseline.large_model,
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
        backend:           active_backend,
        base_url:          active_url,
        api_key:           active_api_key,
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
