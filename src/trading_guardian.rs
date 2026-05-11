//! Dry-run trading guardian for external AgoraMarketAPI supervision.
//!
//! The guardian is intentionally conservative: it can evaluate risk-reducing
//! rules and format actions, but live write actions are denied unless the
//! config explicitly disables dry-run and opts into live actions.

use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub dry_run: bool,
    #[serde(default)]
    pub allow_live_actions: bool,
    #[serde(default = "default_backend_health_url")]
    pub backend_health_url: String,
    #[serde(default = "default_mcp_url")]
    pub mcp_url: String,
    #[serde(default = "default_mcp_bearer_env")]
    pub mcp_bearer_env: String,
    #[serde(default = "default_health_timeout_secs")]
    pub health_timeout_secs: u64,
    #[serde(default = "default_true")]
    pub audit_enabled: bool,
    #[serde(default = "default_action_whitelist")]
    pub action_whitelist: Vec<ActionKind>,
}

fn default_true() -> bool {
    true
}

fn default_backend_health_url() -> String {
    "https://agoramarketapi.purrtechllc.com/actuator/health".to_string()
}

fn default_mcp_url() -> String {
    "https://agoramarketapi.purrtechllc.com/api/mcp".to_string()
}

fn default_mcp_bearer_env() -> String {
    "AGORAMARKETAPI_MCP_TOKEN".to_string()
}

fn default_health_timeout_secs() -> u64 {
    30
}

fn default_action_whitelist() -> Vec<ActionKind> {
    vec![
        ActionKind::SendTgMessage,
        ActionKind::PauseStrategy,
        ActionKind::ModifyOcoTighten,
        ActionKind::DisablePocStrategy,
    ]
}

impl Default for GuardianConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            allow_live_actions: false,
            backend_health_url: default_backend_health_url(),
            mcp_url: default_mcp_url(),
            mcp_bearer_env: default_mcp_bearer_env(),
            health_timeout_secs: default_health_timeout_secs(),
            audit_enabled: true,
            action_whitelist: default_action_whitelist(),
        }
    }
}

impl GuardianConfig {
    pub fn load() -> Self {
        let path = crate::platform::config_path("trading_guardian.yaml");
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_yaml::from_str(&raw).unwrap_or_else(|err| {
                eprintln!(
                    "[trading_guardian] invalid config at {}: {err}; using safe defaults",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleId {
    R1ConsecutiveLosses,
    R2AgedPositionLoss,
    R3DailyLossGuard,
    R4BackendHealth,
    R5TrailingStopAdvisory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    SendTgMessage,
    PauseStrategy,
    ModifyOcoTighten,
    DisablePocStrategy,
    EnableStrategy,
    PlaceMarketOrder,
}

impl ActionKind {
    pub fn is_live_write(self) -> bool {
        matches!(
            self,
            ActionKind::PauseStrategy
                | ActionKind::ModifyOcoTighten
                | ActionKind::DisablePocStrategy
                | ActionKind::EnableStrategy
                | ActionKind::PlaceMarketOrder
        )
    }

    pub fn expands_risk(self) -> bool {
        matches!(
            self,
            ActionKind::EnableStrategy | ActionKind::PlaceMarketOrder
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianAction {
    pub rule: RuleId,
    pub severity: Severity,
    pub kind: ActionKind,
    pub message: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizationDecision {
    pub allowed: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianRunReport {
    pub run_id: String,
    pub enabled: bool,
    pub dry_run: bool,
    pub evaluated_rules: Vec<RuleId>,
    pub proposed_actions: Vec<GuardianAction>,
    pub authorization: Vec<AuthorizationDecision>,
}

#[derive(Debug, Clone)]
pub struct BackendHealthInput {
    pub healthy: bool,
    pub error: Option<String>,
}

pub fn evaluate_backend_health(input: &BackendHealthInput) -> Option<GuardianAction> {
    if input.healthy {
        return None;
    }
    Some(GuardianAction {
        rule: RuleId::R4BackendHealth,
        severity: Severity::Critical,
        kind: ActionKind::SendTgMessage,
        target: None,
        message: format!(
            "BACKEND DOWN. Health check failed: {}. Suggested next step: ssh ubuntu@141.148.142.175 and inspect AgoraMarketAPI service/logs.",
            input.error.as_deref().unwrap_or("unknown error")
        ),
    })
}

#[allow(dead_code)]
pub fn is_poc_strategy(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("nosl") || lower.contains("poc")
}

#[allow(dead_code)]
pub fn r3_disable_candidates<'a>(
    strategies: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<&'a str> {
    strategies
        .into_iter()
        .filter_map(|(id, name)| is_poc_strategy(name).then_some(id))
        .collect()
}

pub fn authorize_action(config: &GuardianConfig, action: &GuardianAction) -> AuthorizationDecision {
    if action.kind.expands_risk() {
        return AuthorizationDecision {
            allowed: false,
            reason: format!(
                "{:?} denied: guardian may not expand risk or open exposure",
                action.kind
            ),
        };
    }
    if !config.action_whitelist.contains(&action.kind) {
        return AuthorizationDecision {
            allowed: false,
            reason: format!("{:?} denied: not present in action_whitelist", action.kind),
        };
    }
    if action.kind.is_live_write() && (config.dry_run || !config.allow_live_actions) {
        return AuthorizationDecision {
            allowed: false,
            reason: format!(
                "{:?} denied: dry_run={} allow_live_actions={}",
                action.kind, config.dry_run, config.allow_live_actions
            ),
        };
    }
    AuthorizationDecision {
        allowed: true,
        reason: format!("{:?} allowed by guardian config", action.kind),
    }
}

async fn probe_backend_health(config: &GuardianConfig) -> BackendHealthInput {
    let client = reqwest::Client::new();
    let result = client
        .get(&config.backend_health_url)
        .timeout(Duration::from_secs(config.health_timeout_secs))
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => BackendHealthInput {
            healthy: true,
            error: None,
        },
        Ok(resp) => BackendHealthInput {
            healthy: false,
            error: Some(format!("HTTP {}", resp.status())),
        },
        Err(err) => BackendHealthInput {
            healthy: false,
            error: Some(err.to_string()),
        },
    }
}

#[allow(dead_code)]
pub async fn run_once(config: GuardianConfig) -> GuardianRunReport {
    run_once_with_backend(config, None).await
}

pub async fn run_once_with_backend(
    config: GuardianConfig,
    backend_override: Option<BackendHealthInput>,
) -> GuardianRunReport {
    let mut proposed_actions = Vec::new();
    let backend = match backend_override {
        Some(input) => input,
        None => probe_backend_health(&config).await,
    };
    if let Some(action) = evaluate_backend_health(&backend) {
        proposed_actions.push(action);
    }

    let authorization = proposed_actions
        .iter()
        .map(|action| authorize_action(&config, action))
        .collect();

    GuardianRunReport {
        run_id: format!(
            "sirin-trading-guardian-{}",
            Utc::now().format("%Y%m%dT%H%M%SZ")
        ),
        enabled: config.enabled,
        dry_run: config.dry_run,
        evaluated_rules: vec![RuleId::R4BackendHealth],
        proposed_actions,
        authorization,
    }
}

pub async fn run_cli(args: &[String]) -> i32 {
    let env_file = crate::platform::app_data_dir().join(".env");
    if env_file.exists() {
        let _ = dotenvy::from_path(&env_file);
    } else {
        let _ = dotenvy::dotenv();
    }

    let config = GuardianConfig::load();
    let simulate_backend_down = args.iter().any(|a| a == "--simulate-backend-down");
    let backend_override = simulate_backend_down.then(|| BackendHealthInput {
        healthy: false,
        error: Some("simulated backend-down drill".to_string()),
    });
    let report = run_once_with_backend(config.clone(), backend_override).await;
    if config.audit_enabled {
        if let Err(err) = write_audit_report(&report) {
            eprintln!("[trading_guardian] audit write failed: {err}");
        }
    }
    if args.iter().any(|a| a == "--format=json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        println!("Sirin Trading Guardian");
        println!("enabled={} dry_run={}", report.enabled, report.dry_run);
        println!("evaluated_rules={:?}", report.evaluated_rules);
        if report.proposed_actions.is_empty() {
            println!("actions=none");
        } else {
            for (action, decision) in report.proposed_actions.iter().zip(&report.authorization) {
                println!(
                    "- {:?} {:?}: {} | allowed={} ({})",
                    action.rule, action.kind, action.message, decision.allowed, decision.reason
                );
            }
        }
    }
    0
}

pub fn audit_dir() -> std::path::PathBuf {
    crate::platform::app_data_dir().join("trading_guardian")
}

pub fn write_audit_report(report: &GuardianRunReport) -> Result<std::path::PathBuf, String> {
    let dir = audit_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    let path = dir.join(format!("{}.json", report.run_id));
    let raw = serde_json::to_string_pretty(report).map_err(|err| err.to_string())?;
    std::fs::write(&path, raw).map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(path)
}

#[allow(dead_code)]
pub async fn call_mcp_tool(
    config: &GuardianConfig,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let token = std::env::var(&config.mcp_bearer_env).ok();
    crate::mcp_client::call_tool_with_bearer(
        &config.mcp_url,
        tool_name,
        arguments,
        token.as_deref(),
    )
    .await
}

#[allow(dead_code)]
pub fn kb_audit_payload(report: &GuardianRunReport) -> serde_json::Value {
    json!({
        "source": "sirin-trading-guardian",
        "enabled": report.enabled,
        "dry_run": report.dry_run,
        "evaluated_rules": report.evaluated_rules,
        "proposed_actions": report.proposed_actions,
        "authorization": report.authorization,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_safe_dry_run() {
        let config = GuardianConfig::default();
        assert!(!config.enabled);
        assert!(config.dry_run);
        assert!(!config.allow_live_actions);
    }

    #[test]
    fn backend_down_creates_critical_tg_action() {
        let action = evaluate_backend_health(&BackendHealthInput {
            healthy: false,
            error: Some("connection refused".to_string()),
        })
        .expect("backend down should propose an alert");
        assert_eq!(action.rule, RuleId::R4BackendHealth);
        assert_eq!(action.severity, Severity::Critical);
        assert_eq!(action.kind, ActionKind::SendTgMessage);
    }

    #[test]
    fn dry_run_denies_live_writes() {
        let config = GuardianConfig::default();
        let action = GuardianAction {
            rule: RuleId::R1ConsecutiveLosses,
            severity: Severity::Warn,
            kind: ActionKind::PauseStrategy,
            message: "pause strategy".to_string(),
            target: Some("566".to_string()),
        };
        let decision = authorize_action(&config, &action);
        assert!(!decision.allowed);
        assert!(decision.reason.contains("dry_run=true"));
    }

    #[test]
    fn guardian_never_allows_risk_expansion() {
        let config = GuardianConfig {
            enabled: true,
            dry_run: false,
            allow_live_actions: true,
            ..GuardianConfig::default()
        };
        let action = GuardianAction {
            rule: RuleId::R3DailyLossGuard,
            severity: Severity::Critical,
            kind: ActionKind::EnableStrategy,
            message: "enable strategy".to_string(),
            target: Some("574".to_string()),
        };
        let decision = authorize_action(&config, &action);
        assert!(!decision.allowed);
        assert!(decision.reason.contains("expand risk"));
    }

    #[test]
    fn r3_filters_only_poc_strategy_names() {
        let candidates = r3_disable_candidates([
            ("1", "NoSL-BTC-POC-v1"),
            ("2", "MEI-LONG-BTC-1h-v2"),
            ("3", "risk-poc-lab"),
        ]);
        assert_eq!(candidates, vec!["1", "3"]);
    }

    #[test]
    fn kb_payload_contains_dry_run_evidence() {
        let report = GuardianRunReport {
            run_id: "test-run".to_string(),
            enabled: true,
            dry_run: true,
            evaluated_rules: vec![RuleId::R4BackendHealth],
            proposed_actions: Vec::new(),
            authorization: Vec::new(),
        };
        let payload = kb_audit_payload(&report);
        assert_eq!(payload["source"], "sirin-trading-guardian");
        assert_eq!(payload["dry_run"], true);
    }

    #[tokio::test]
    async fn simulated_backend_down_is_auditable() {
        let config = GuardianConfig::default();
        let report = run_once_with_backend(
            config,
            Some(BackendHealthInput {
                healthy: false,
                error: Some("simulated backend-down drill".to_string()),
            }),
        )
        .await;
        assert_eq!(report.proposed_actions.len(), 1);
        assert_eq!(report.proposed_actions[0].rule, RuleId::R4BackendHealth);

        let path = write_audit_report(&report).expect("audit write should succeed in test_data");
        assert!(path.exists());
        let raw = std::fs::read_to_string(path).expect("audit file should be readable");
        assert!(raw.contains("simulated backend-down drill"));
    }
}
