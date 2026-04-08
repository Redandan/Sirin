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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        }
    }
}

/// Collection of channels an agent may communicate through.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramChannelConfig>,
}

// ── Actions ───────────────────────────────────────────────────────────────────

/// Per-agent research-agent toggle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchAgentConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ResearchAgentConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Per-agent coding-agent enable flag.
/// (Detailed coding config lives in config/persona.yaml → coding_agent.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingEnabledConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CodingEnabledConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Capability flags for a single agent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActionsConfig {
    #[serde(default)]
    pub coding_agent: CodingEnabledConfig,
    #[serde(default)]
    pub research_agent: ResearchAgentConfig,
}

// ── Agent ─────────────────────────────────────────────────────────────────────

/// Complete configuration for one AI agent instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    // ── 3. 能力 Actions ───────────────────────────────────────────────────────
    #[serde(default)]
    pub actions: ActionsConfig,

    // ── 4. 遠端 AI 控制 ────────────────────────────────────────────────────────
    #[serde(default)]
    pub disable_remote_ai: bool,

    // ── 5. 目標 Objectives ────────────────────────────────────────────────────
    /// Per-agent objectives used by the behavior engine.
    /// If empty, falls back to the global Persona.objectives.
    #[serde(default)]
    pub objectives: Vec<String>,
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
            actions: ActionsConfig::default(),
        }
    }
}

// ── File ──────────────────────────────────────────────────────────────────────

/// Top-level wrapper for `config/agents.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentsFile {
    pub agents: Vec<AgentConfig>,
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

fn default_true() -> bool {
    true
}
