//! Multi-agent configuration — `config/agents.yaml`.
//!
//! Each [`AgentConfig`] entry represents one independently-running AI agent
//! with its own identity, goals, optional channel bindings, capability flags.
//! The [`AgentsFile`] wrapper is the top-level YAML document.

use std::fs;

use serde::{Deserialize, Serialize};

use crate::persona::{Identity, ProfessionalTone, ResponseStyle};

// ── Channel ───────────────────────────────────────────────────────────────────

/// Telegram-specific channel parameters.
///
/// Values may be literal strings (`"12345678"`) or env-var references
/// (`"${TG_API_ID}"`).  The runtime will resolve references at launch time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    /// Telegram App API ID (integer as string, or `"${VAR}"`).
    pub api_id: String,
    /// Telegram App API hash.
    pub api_hash: String,
    /// Phone number in international format, or `"${VAR}"`.
    pub phone: String,
    /// Path to the SQLite session file (unique per agent).
    pub session_file: String,
    /// Reply to private messages?
    #[serde(default = "default_true")]
    pub reply_private: bool,
    /// Reply inside group chats?
    #[serde(default)]
    pub reply_groups: bool,
    /// Group / channel bare IDs to monitor (empty = all accessible groups).
    #[serde(default)]
    pub group_ids: Vec<i64>,
    /// Enable automatic AI reply when a message arrives.
    #[serde(default)]
    pub auto_reply: bool,
    /// Optional message sent to the account's Saved Messages on startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_msg: Option<String>,
    /// If true, AI draft is saved as PendingReply and requires human approval before sending.
    #[serde(default)]
    pub require_confirmation: bool,
}

impl Default for TelegramChannelConfig {
    fn default() -> Self {
        Self {
            api_id: "${TG_API_ID}".to_string(),
            api_hash: "${TG_API_HASH}".to_string(),
            phone: "${TG_PHONE}".to_string(),
            session_file: "data/sessions/agent.session".to_string(),
            reply_private: true,
            reply_groups: false,
            group_ids: Vec::new(),
            auto_reply: false,
            startup_msg: None,
            require_confirmation: false,
        }
    }
}

/// Collection of channels an agent may communicate through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChannelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramChannelConfig>,
    /// Teams integration — UI placeholder only (not yet implemented).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teams: Option<TeamsChannelConfig>,
}

/// Microsoft Teams channel placeholder (not yet implemented).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TeamsChannelConfig {
    /// Teams webhook URL or `"${VAR}"` reference.
    #[serde(default)]
    pub webhook_url: String,
}

// ── Platform detection ────────────────────────────────────────────────────────

/// The primary platform an agent uses (inferred at runtime, not stored in YAML).
#[derive(Debug, Clone, PartialEq)]
pub enum AgentPlatform {
    Telegram,
    /// Teams placeholder — UI only, no real integration.
    Teams,
    UiOnly,
}

// ── Human behavior simulation ─────────────────────────────────────────────────

/// A single break period within a work day (e.g. lunch break).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakPeriod {
    /// Human-readable label (e.g. "午休").
    pub name: String,
    /// Start time in "HH:MM" format (local time per utc_offset_hours).
    pub start: String,
    /// End time in "HH:MM" format.
    pub end: String,
}

/// Work-schedule constraints used by the human-behavior engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkSchedule {
    /// UTC offset in whole hours (e.g. 8 for CST/UTC+8).
    #[serde(default)]
    pub utc_offset_hours: i32,
    /// Work day start time in "HH:MM" local time.
    #[serde(default = "default_work_start")]
    pub work_start: String,
    /// Work day end time in "HH:MM" local time.
    #[serde(default = "default_work_end")]
    pub work_end: String,
    /// Active weekdays: 1=Mon … 7=Sun.  Defaults to Mon–Fri.
    #[serde(default = "default_work_days")]
    pub work_days: Vec<u8>,
    /// Intra-day break windows where replies are suppressed.
    #[serde(default)]
    pub breaks: Vec<BreakPeriod>,
}

impl Default for WorkSchedule {
    fn default() -> Self {
        Self {
            utc_offset_hours: 8,
            work_start: default_work_start(),
            work_end: default_work_end(),
            work_days: default_work_days(),
            breaks: Vec::new(),
        }
    }
}

/// Simulates human-like reply timing: random delay, frequency caps, work schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HumanBehaviorConfig {
    /// Master switch.  When false all other fields are ignored.
    #[serde(default)]
    pub enabled: bool,
    /// Minimum random delay before a reply is sent (seconds).
    #[serde(default = "default_min_delay")]
    pub min_reply_delay_secs: u64,
    /// Maximum random delay before a reply is sent (seconds).
    #[serde(default = "default_max_delay")]
    pub max_reply_delay_secs: u64,
    /// Maximum outgoing messages per hour (0 = unlimited).
    #[serde(default = "default_max_per_hour")]
    pub max_messages_per_hour: u32,
    /// Maximum outgoing messages per day (0 = unlimited).
    #[serde(default = "default_max_per_day")]
    pub max_messages_per_day: u32,
    /// Optional work-schedule constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_schedule: Option<WorkSchedule>,
}

impl Default for HumanBehaviorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_reply_delay_secs: default_min_delay(),
            max_reply_delay_secs: default_max_delay(),
            max_messages_per_hour: default_max_per_hour(),
            max_messages_per_day: default_max_per_day(),
            work_schedule: None,
        }
    }
}

// ── KPI ───────────────────────────────────────────────────────────────────────

/// Where a KPI metric value comes from.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum KpiSource {
    /// Value is entered manually by the operator.
    #[default]
    Manual,
    /// Value is fetched from the Agora API (TODO: not yet implemented).
    AgoraApi,
}

/// Definition of one KPI metric tracked per agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KpiMetricDef {
    /// Machine-readable key (e.g. "conversion_rate").
    pub key: String,
    /// Display label (e.g. "轉化率").
    pub label: String,
    /// Unit string appended after the value (e.g. "%", "次").
    #[serde(default)]
    pub unit: String,
    /// Data source for this metric.
    #[serde(default)]
    pub source: KpiSource,
    /// Agora API endpoint URL (only used when source = AgoraApi).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    /// Env-var name that holds the API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// KPI tracking configuration for one agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct KpiConfig {
    #[serde(default)]
    pub metrics: Vec<KpiMetricDef>,
}

// ── Actions ───────────────────────────────────────────────────────────────────


// ── Memory policy ─────────────────────────────────────────────────────────────

/// Controls which agents may deliver confidential memories to this agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemoryPolicy {
    /// Agent IDs allowed to call `confidential_handoff` targeting this agent.
    /// Empty = no agent is trusted (confidential handoff disabled).
    #[serde(default)]
    pub trusted_senders: Vec<String>,
}

// ── Agent ─────────────────────────────────────────────────────────────────────

/// Per-agent LLM provider override.
///
/// When present on an [`AgentConfig`], that agent will use a separate LLM
/// (e.g. Anthropic Claude) instead of the process-wide default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentLlmOverride {
    /// Backend name: `"anthropic"`, `"lmstudio"`, `"gemini"`, or `"ollama"`.
    pub backend: String,
    /// Model ID recognised by the backend (e.g. `"claude-sonnet-4-6"`).
    pub model: String,
    /// Name of the environment variable that holds the API key.
    /// Resolved at call time via `std::env::var`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// An external cloud AI provider that can participate in meetings but is NOT
/// a regular agent (no Telegram channel, KPI tracking, or sidebar card).
///
/// Configured under `external_providers:` in `config/agents.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalAiProvider {
    /// Unique slug identifier (e.g. `"claude"`).
    pub id: String,
    /// Display name shown in the meeting UI (e.g. `"Claude"`).
    pub name: String,
    /// Backend name: `"anthropic"`, `"gemini"`, `"lmstudio"`, or `"ollama"`.
    pub backend: String,
    /// Model ID recognised by the backend (e.g. `"claude-sonnet-4-6"`).
    pub model: String,
    /// Name of the environment variable that holds the API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl ExternalAiProvider {
    /// Resolve into an [`LlmConfig`] for use with [`ChatRequest::llm_override`].
    pub fn resolve_llm_config(&self) -> crate::llm::LlmConfig {
        let api_key = self.api_key_env.as_deref()
            .and_then(|env| std::env::var(env).ok())
            .filter(|v| !v.trim().is_empty());
        crate::llm::LlmConfig::for_override(&self.backend, &self.model, api_key)
    }
}

/// Complete configuration for one AI agent instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Unique slug identifier (lowercase, no spaces).
    pub id: String,
    /// Whether this agent is active and should be started by the runtime.
    #[serde(default = "default_true")]
    pub enabled: bool,

    // ── 1. 身分 Identity ──────────────────────────────────────────────────────
    pub identity: Identity,
    #[serde(default)]
    pub response_style: ResponseStyle,

    // ── 2. 通訊平台 Channel ────────────────────────────────────────────────────
    /// Optional channel config.  `None` = no external channel (UI / test only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelConfig>,

    // ── 3. 技能黑名單 Skills ──────────────────────────────────────────────────
    /// 此 agent 被停用的技能 ID 清單（對應 config/skills/*.yaml 的 id）。
    /// 空（預設）= 使用所有可用技能；列出的 ID = 此助手無法使用該技能。
    #[serde(default)]
    pub disabled_skills: Vec<String>,

    // ── 4. 遠端 AI 控制 ────────────────────────────────────────────────────────
    #[serde(default)]
    pub disable_remote_ai: bool,

    // ── 5. 目標 Objectives ────────────────────────────────────────────────────
    /// Per-agent objectives used by the behavior engine.
    /// If empty, falls back to the global Persona.objectives.
    #[serde(default)]
    pub objectives: Vec<String>,

    // ── 6. 人類行為模擬 ────────────────────────────────────────────────────────
    #[serde(default)]
    pub human_behavior: HumanBehaviorConfig,

    // ── 7. KPI 追蹤 ───────────────────────────────────────────────────────────
    #[serde(default)]
    pub kpi: KpiConfig,

    // ── 8. 記憶存取政策 ────────────────────────────────────────────────────────
    #[serde(default)]
    pub memory_policy: MemoryPolicy,

    // ── 9. LLM 覆寫（per-agent provider，如 Anthropic Claude）────────────────
    /// When set, this agent uses its own LLM instead of the process-wide default.
    /// Example: `{ backend: anthropic, model: claude-sonnet-4-6, api_key_env: ANTHROPIC_API_KEY }`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_override: Option<AgentLlmOverride>,
}

impl AgentConfig {
    /// Create a blank agent skeleton with sensible defaults.
    pub fn new_default(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            enabled: false,
            identity: Identity {
                name: name.into(),
                professional_tone: ProfessionalTone::Brief,
            },
            response_style: ResponseStyle::default(),
            disable_remote_ai: false,
            objectives: Vec::new(),
            channel: None,
            disabled_skills: Vec::new(),
            human_behavior: HumanBehaviorConfig::default(),
            kpi: KpiConfig::default(),
            memory_policy: MemoryPolicy::default(),
            llm_override: None,
        }
    }

    /// Resolve the per-agent LLM override into an `LlmConfig`, if any.
    ///
    /// Reads the API key from the named environment variable.
    /// Returns `None` when no override is configured.
    pub fn resolve_llm_override(&self) -> Option<crate::llm::LlmConfig> {
        let ov = self.llm_override.as_ref()?;
        let api_key = ov
            .api_key_env
            .as_deref()
            .and_then(|env| std::env::var(env).ok())
            .filter(|v| !v.trim().is_empty());
        Some(crate::llm::LlmConfig::for_override(&ov.backend, &ov.model, api_key))
    }

    /// Returns true if at least one coding skill exists and is not disabled.
    pub fn can_code(&self) -> bool {
        let all = crate::skills::list_skills();
        all.iter()
            .filter(|s| s.category == "coding")
            .any(|s| !self.disabled_skills.contains(&s.id))
    }

    /// Returns true if at least one research skill exists and is not disabled.
    pub fn can_research(&self) -> bool {
        let all = crate::skills::list_skills();
        all.iter()
            .filter(|s| s.category == "research")
            .any(|s| !self.disabled_skills.contains(&s.id))
    }

    /// Infer the primary platform from channel config (not stored in YAML).
    pub fn platform(&self) -> AgentPlatform {
        match &self.channel {
            Some(ch) if ch.telegram.is_some() => AgentPlatform::Telegram,
            Some(ch) if ch.teams.is_some() => AgentPlatform::Teams,
            _ => AgentPlatform::UiOnly,
        }
    }
}

// ── File ──────────────────────────────────────────────────────────────────────

/// Top-level wrapper for `config/agents.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentsFile {
    pub agents: Vec<AgentConfig>,
    /// Maximum number of agents that may be active simultaneously (UI cap).
    #[serde(default = "default_max_agents")]
    pub max_agents: usize,
    /// External cloud AI providers available as meeting participants.
    /// These are NOT regular agents — they have no channel, KPI, or sidebar card.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_providers: Vec<ExternalAiProvider>,
}

impl Default for AgentsFile {
    fn default() -> Self {
        // Provide a sensible first-run skeleton so the sidebar is never empty.
        let mut agent = AgentConfig::new_default("assistant_1", "助手1");
        agent.enabled = true;
        agent.channel = Some(ChannelConfig {
            telegram: Some(TelegramChannelConfig::default()),
            teams: None,
        });
        Self {
            agents: vec![agent],
            max_agents: default_max_agents(),
            external_providers: Vec::new(),
        }
    }
}

impl AgentsFile {
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = fs::read_to_string("config/agents.yaml")?;
        Ok(serde_yaml::from_str(&content)?)
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let yaml = serde_yaml::to_string(self)?;
        fs::write("config/agents.yaml", yaml)?;
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn default_true() -> bool { true }
fn default_min_delay() -> u64 { 30 }
fn default_max_delay() -> u64 { 180 }
fn default_max_per_hour() -> u32 { 20 }
fn default_max_per_day() -> u32 { 100 }
fn default_max_agents() -> usize { 2 }
fn default_work_start() -> String { "09:00".to_string() }
fn default_work_end() -> String { "18:00".to_string() }
fn default_work_days() -> Vec<u8> { vec![1, 2, 3, 4, 5] }
