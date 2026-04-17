//! Persona configuration — identity, response style, ROI thresholds,
//! coding-agent config, and the YAML-backed `Persona` struct consumed across
//! the app.
//!
//! ## Concurrency
//! - `Persona::cached()` returns a clone from a process-wide `RwLock<Persona>`;
//!   concurrent readers don't block each other.  Writers (`reload_cache`) take
//!   the write lock briefly.
//! - Hot paths **must** call `cached()`, not `load()` — `load()` hits disk.
//!
//! ## Submodules
//! - [`behavior`] — action-tier classifier and response-draft generator.
//! - [`task_tracker`] — append-only event log (`TaskEntry` + `TaskTracker`).

mod behavior;
mod task_tracker;

pub use behavior::{ActionTier, BehaviorDecision, BehaviorEngine, IncomingMessage};
pub use task_tracker::{TaskEntry, TaskTracker};

use std::fs;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfessionalTone {
    Brief,
    Detailed,
    Casual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub professional_tone: ProfessionalTone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoiThresholds {
    pub min_usd_to_notify: f64,
    pub min_usd_to_call_remote_llm: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseStyle {
    #[serde(default = "default_voice")]
    pub voice: String,
    #[serde(default = "default_ack_prefix")]
    pub ack_prefix: String,
    #[serde(default = "default_compliance_line")]
    pub compliance_line: String,
}

impl Default for ResponseStyle {
    fn default() -> Self {
        Self {
            voice: default_voice(),
            ack_prefix: default_ack_prefix(),
            compliance_line: default_compliance_line(),
        }
    }
}

/// Configuration for the local AI Coding Agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingAgentConfig {
    /// Whether the coding agent is enabled at all.
    #[serde(default = "default_coding_enabled")]
    pub enabled: bool,
    /// Root directory that file operations are allowed to touch.
    /// Relative paths are resolved from the process working directory.
    #[serde(default = "default_coding_project_root")]
    pub project_root: String,
    /// Skip user confirmation for read-only operations.
    #[serde(default = "default_true")]
    pub auto_approve_reads: bool,
    /// When false, the UI will show a confirmation dialog before any file write.
    #[serde(default)]
    pub auto_approve_writes: bool,
    /// Shell commands (exact prefix match) the agent is allowed to execute.
    #[serde(default = "default_allowed_commands")]
    pub allowed_commands: Vec<String>,
    /// Maximum number of ReAct loop iterations per task.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Maximum bytes that a single file write may contain.
    #[serde(default = "default_max_file_write_bytes")]
    pub max_file_write_bytes: usize,
}

fn default_coding_enabled() -> bool {
    true
}
fn default_coding_project_root() -> String {
    ".".to_string()
}
fn default_true() -> bool {
    true
}
fn default_allowed_commands() -> Vec<String> {
    vec![
        "cargo check".to_string(),
        "cargo test".to_string(),
        "cargo build --release".to_string(),
    ]
}
fn default_max_iterations() -> usize {
    10
}
fn default_max_file_write_bytes() -> usize {
    102_400
}

impl Default for CodingAgentConfig {
    fn default() -> Self {
        Self {
            enabled: default_coding_enabled(),
            project_root: default_coding_project_root(),
            auto_approve_reads: default_true(),
            auto_approve_writes: false,
            allowed_commands: default_allowed_commands(),
            max_iterations: default_max_iterations(),
            max_file_write_bytes: default_max_file_write_bytes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub identity: Identity,
    pub objectives: Vec<String>,
    pub roi_thresholds: RoiThresholds,
    #[serde(default)]
    pub response_style: ResponseStyle,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub coding_agent: CodingAgentConfig,
    /// When true, escalation to a remote/large model is suppressed at runtime.
    /// The main LLM backend (set via `LLM_PROVIDER` env var) is still used.
    #[serde(default)]
    pub disable_remote_ai: bool,
}

fn default_version() -> String {
    "1.0".to_string()
}

fn default_voice() -> String {
    "自然、禮貌、專業".to_string()
}

fn default_ack_prefix() -> String {
    "已收到你的訊息。".to_string()
}

fn default_compliance_line() -> String {
    "我會按照你的要求處理。".to_string()
}

/// Process-wide cached Persona. Loaded once on first access, avoids
/// repeated YAML reads from disk on every tool call.
static PERSONA_CACHE: std::sync::OnceLock<std::sync::RwLock<Persona>> = std::sync::OnceLock::new();

impl Persona {
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = fs::read_to_string(crate::platform::config_path("persona.yaml"))?;
        let persona = serde_yaml::from_str(&content)?;
        Ok(persona)
    }

    /// Return a cached clone of the Persona. First call reads from disk;
    /// subsequent calls return the cached value without I/O.
    pub fn cached() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let rw = PERSONA_CACHE.get_or_init(|| {
            let p = Self::load().unwrap_or_else(|_| Self::default_fallback());
            std::sync::RwLock::new(p)
        });
        Ok(rw.read().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// Force-reload the Persona from disk and update the cache.
    pub fn reload_cache() {
        if let Ok(p) = Self::load() {
            if let Some(rw) = PERSONA_CACHE.get() {
                *rw.write().unwrap_or_else(|e| e.into_inner()) = p;
            }
        }
    }

    /// Minimal fallback when config/persona.yaml is missing or invalid.
    fn default_fallback() -> Self {
        serde_yaml::from_str(
            "identity:\n  name: Sirin\n  professional_tone: brief\nobjectives: []\n\
             roi_thresholds:\n  min_usd_to_notify: 5.0\n  min_usd_to_call_remote_llm: 25.0\n",
        )
        .expect("hardcoded YAML must parse")
    }

    pub fn name(&self) -> &str {
        &self.identity.name
    }

    pub fn objective_match(&self, text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        self.objectives
            .iter()
            .find(|o| lower.contains(&o.to_lowercase()))
            .cloned()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_persona() -> Persona {
        Persona {
            identity: Identity {
                name: "Sirin".to_string(),
                professional_tone: ProfessionalTone::Brief,
            },
            objectives: vec!["Monitor Agora".to_string(), "Maintain VIPs".to_string()],
            roi_thresholds: RoiThresholds {
                min_usd_to_notify: 5.0,
                min_usd_to_call_remote_llm: 25.0,
            },
            response_style: ResponseStyle::default(),
            version: "1.0".to_string(),
            description: "test".to_string(),
            coding_agent: CodingAgentConfig::default(),
            disable_remote_ai: false,
        }
    }

    #[test]
    fn persona_load_reads_config_yaml() {
        let p = Persona::load();
        assert!(
            p.is_ok(),
            "config/persona.yaml should be loadable: {:?}",
            p.err()
        );
        let p = p.unwrap();
        assert!(
            !p.identity.name.is_empty(),
            "persona name must not be empty"
        );
    }

    #[test]
    fn persona_yaml_roundtrip() {
        let p = test_persona();
        let yaml = serde_yaml::to_string(&p).expect("serialization should not fail");
        let reloaded: Persona =
            serde_yaml::from_str(&yaml).expect("deserialization should not fail");
        assert_eq!(reloaded.identity.name, p.identity.name);
        assert_eq!(reloaded.objectives, p.objectives);
        assert!(
            (reloaded.roi_thresholds.min_usd_to_notify - p.roi_thresholds.min_usd_to_notify).abs()
                < f64::EPSILON
        );
    }
}
