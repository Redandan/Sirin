//! Configuration diagnostics — detects misconfigs, missing services,
//! and suboptimal settings.  Shared by startup, UI, and skill entry points.
//!
//! ## AI-assisted fixing
//! `ai_analyze()` asks the main LLM to produce a structured fix proposal
//! ([`AiAdvice`]).  `apply_fixes()` applies approved fixes with a file
//! whitelist + `.bak.TIMESTAMP` backups.  Never writes to `.env` or code.

use std::path::Path;

/// Files that `apply_fixes` is allowed to modify.  Hardcoded — never bypass.
const ALLOWED_FILES: &[&str] = &[
    "config/llm.yaml",
    "config/persona.yaml",
];

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Ok,
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ok      => "OK",
            Self::Info    => "INFO",
            Self::Warning => "WARN",
            Self::Error   => "ERROR",
        }
    }
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Ok      => "[OK]",
            Self::Info    => "[i]",
            Self::Warning => "[!]",
            Self::Error   => "[X]",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigIssue {
    pub severity: Severity,
    pub category: &'static str,
    pub message: String,
    pub suggestion: Option<String>,
}

// ── Core diagnostics ─────────────────────────────────────────────────────────

pub fn run_diagnostics() -> Vec<ConfigIssue> {
    let mut issues = Vec::new();

    check_config_files(&mut issues);
    check_llm_config(&mut issues);
    check_router(&mut issues);
    check_vision(&mut issues);
    check_model_roles(&mut issues);
    check_persona(&mut issues);
    check_coding_agent(&mut issues);
    check_external_tools(&mut issues);

    // Sort by severity (errors first)
    issues.sort_by(|a, b| b.severity.cmp(&a.severity));
    issues
}

// ── Individual checks ────────────────────────────────────────────────────────

fn check_config_files(issues: &mut Vec<ConfigIssue>) {
    for (path, name) in [
        ("config/persona.yaml", "Persona"),
        ("config/agents.yaml", "Agents"),
    ] {
        if !Path::new(path).exists() {
            issues.push(ConfigIssue {
                severity: Severity::Error,
                category: "Config",
                message: format!("{name} config not found: {path}"),
                suggestion: Some("Run Sirin once to generate default config, or create manually.".into()),
            });
        } else {
            issues.push(ConfigIssue {
                severity: Severity::Ok,
                category: "Config",
                message: format!("{name} config: {path}"),
                suggestion: None,
            });
        }
    }
}

fn check_llm_config(issues: &mut Vec<ConfigIssue>) {
    let env_provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
    let env_model = match env_provider.as_str() {
        "gemini" | "google" => std::env::var("GEMINI_MODEL").unwrap_or_default(),
        "anthropic" | "claude" => std::env::var("ANTHROPIC_MODEL").unwrap_or_default(),
        _ => std::env::var("OLLAMA_MODEL").unwrap_or_default(),
    };

    // Check llm.yaml override conflict
    let yaml = crate::llm::LlmUiConfig::load();
    if !yaml.main_model.is_empty() && !env_model.is_empty() && yaml.main_model != env_model {
        issues.push(ConfigIssue {
            severity: Severity::Warning,
            category: "LLM",
            message: format!(
                ".env model={env_model} but llm.yaml overrides to {}",
                yaml.main_model
            ),
            suggestion: Some(format!(
                "Edit config/llm.yaml and clear main_model, or update .env to match. \
                 Currently using: {}",
                yaml.main_model
            )),
        });
    }

    // Check if using expensive model without role split
    let effective_main = if !yaml.main_model.is_empty() { &yaml.main_model } else { &env_model };
    let is_expensive = effective_main.contains("pro") || effective_main.contains("opus");
    if is_expensive && yaml.coding_model.is_empty() && yaml.large_model.is_empty() {
        issues.push(ConfigIssue {
            severity: Severity::Warning,
            category: "LLM",
            message: format!("Using expensive model '{effective_main}' for ALL roles (chat/coding/large)"),
            suggestion: Some(
                "Set coding_model to a cheaper model (e.g. gemini-2.5-flash) in llm.yaml. \
                 Reserve the expensive model for large_model only.".into()
            ),
        });
    }

    // Check API key
    if env_provider == "gemini" {
        if std::env::var("GEMINI_API_KEY").unwrap_or_default().is_empty() {
            issues.push(ConfigIssue {
                severity: Severity::Error,
                category: "LLM",
                message: "GEMINI_API_KEY not set but LLM_PROVIDER=gemini".into(),
                suggestion: Some("Add GEMINI_API_KEY to .env file.".into()),
            });
        } else {
            issues.push(ConfigIssue {
                severity: Severity::Ok,
                category: "LLM",
                message: format!("Gemini configured: {effective_main}"),
                suggestion: None,
            });
        }
    }
}

fn check_router(issues: &mut Vec<ConfigIssue>) {
    let router_provider = std::env::var("ROUTER_LLM_PROVIDER").unwrap_or_default();
    if router_provider.is_empty() {
        issues.push(ConfigIssue {
            severity: Severity::Info,
            category: "Router",
            message: "No separate router model — using main model for intent classification".into(),
            suggestion: Some(
                "Set ROUTER_LLM_PROVIDER=lmstudio or ollama in .env to keep routing local and save cloud quota.".into()
            ),
        });
        return;
    }

    // Check if the local backend is reachable
    let base_url = match router_provider.as_str() {
        "ollama" => std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".into()),
        "lmstudio" | "lm_studio" => std::env::var("LM_STUDIO_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:1234/v1".into()),
        _ => return,
    };

    let reachable = std::net::TcpStream::connect_timeout(
        &url_to_addr(&base_url).unwrap_or_else(|| "127.0.0.1:11434".parse().unwrap()),
        std::time::Duration::from_secs(2),
    ).is_ok();

    let router_model = std::env::var("ROUTER_MODEL")
        .or_else(|_| std::env::var("LM_STUDIO_MODEL"))
        .unwrap_or_else(|_| "?".into());

    if reachable {
        issues.push(ConfigIssue {
            severity: Severity::Ok,
            category: "Router",
            message: format!("Router: {router_provider} ({router_model}) at {base_url}"),
            suggestion: None,
        });
    } else {
        issues.push(ConfigIssue {
            severity: Severity::Error,
            category: "Router",
            message: format!(
                "Router backend '{router_provider}' not reachable at {base_url}"
            ),
            suggestion: Some(format!(
                "Start {router_provider} or remove ROUTER_LLM_PROVIDER from .env to use cloud model for routing."
            )),
        });
    }
}

fn check_vision(issues: &mut Vec<ConfigIssue>) {
    // Check if any vision-capable model is available via the fleet
    let fleet = crate::llm::shared_fleet();
    if fleet.has_capability(&crate::llm::ModelCapability::Vision) {
        let names: Vec<&str> = fleet.classified_models.iter()
            .filter(|m| m.has(&crate::llm::ModelCapability::Vision))
            .map(|m| m.info.name.as_str())
            .collect();
        issues.push(ConfigIssue {
            severity: Severity::Ok,
            category: "Vision",
            message: format!("Vision models available: {}", names.join(", ")),
            suggestion: None,
        });
    } else if std::env::var("LLM_PROVIDER").unwrap_or_default() != "gemini" {
        issues.push(ConfigIssue {
            severity: Severity::Warning,
            category: "Vision",
            message: "No vision-capable models detected in local fleet".into(),
            suggestion: Some(
                "Install a vision model (llava, moondream, qwen2.5-vl, gemma-4) in Ollama/LM Studio, \
                 or use Gemini which has built-in vision.".into()
            ),
        });
    }

    // Check if current main model has vision (Gemini/GPT-4o do, Ollama text models don't)
    let provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
    if provider == "gemini" {
        issues.push(ConfigIssue {
            severity: Severity::Ok,
            category: "Vision",
            message: "Gemini backend supports vision natively (screenshot_analyze available)".into(),
            suggestion: None,
        });
    }
}

fn check_model_roles(issues: &mut Vec<ConfigIssue>) {
    let yaml = crate::llm::LlmUiConfig::load();
    let coding = std::env::var("CODING_MODEL").ok()
        .or_else(|| if yaml.coding_model.is_empty() { None } else { Some(yaml.coding_model.clone()) });
    let large = std::env::var("LARGE_MODEL").ok()
        .or_else(|| if yaml.large_model.is_empty() { None } else { Some(yaml.large_model.clone()) });

    if coding.is_none() {
        issues.push(ConfigIssue {
            severity: Severity::Info,
            category: "Roles",
            message: "No dedicated coding model — using main model for code tasks".into(),
            suggestion: Some("Set CODING_MODEL in .env or coding_model in llm.yaml for better cost control.".into()),
        });
    }
    if large.is_none() {
        issues.push(ConfigIssue {
            severity: Severity::Info,
            category: "Roles",
            message: "No dedicated large model — using main model for deep reasoning".into(),
            suggestion: Some("Set LARGE_MODEL in .env or large_model in llm.yaml (e.g. gemini-2.5-pro).".into()),
        });
    }
}

fn check_persona(issues: &mut Vec<ConfigIssue>) {
    match crate::persona::Persona::load() {
        Ok(p) => {
            if p.objectives.is_empty() {
                issues.push(ConfigIssue {
                    severity: Severity::Info,
                    category: "Persona",
                    message: "No persona objectives set".into(),
                    suggestion: Some("Add objectives in config/persona.yaml to guide agent behavior.".into()),
                });
            }
            let roi = &p.roi_thresholds;
            if roi.min_usd_to_notify > 50.0 {
                issues.push(ConfigIssue {
                    severity: Severity::Warning,
                    category: "Persona",
                    message: format!("ROI notify threshold is very high (${:.0}) — agent may ignore most messages", roi.min_usd_to_notify),
                    suggestion: Some("Lower min_usd_to_notify in persona.yaml (recommended: 1-10).".into()),
                });
            }
        }
        Err(_) => {
            issues.push(ConfigIssue {
                severity: Severity::Error,
                category: "Persona",
                message: "Failed to load persona config".into(),
                suggestion: Some("Check config/persona.yaml for YAML syntax errors.".into()),
            });
        }
    }
}

fn check_coding_agent(issues: &mut Vec<ConfigIssue>) {
    if let Ok(p) = crate::persona::Persona::load() {
        let ca = &p.coding_agent;
        if ca.enabled && ca.allowed_commands.len() < 3 {
            issues.push(ConfigIssue {
                severity: Severity::Info,
                category: "Coding",
                message: format!("Coding agent has only {} allowed commands", ca.allowed_commands.len()),
                suggestion: Some("Add more commands to coding_agent.allowed_commands in persona.yaml if needed.".into()),
            });
        }
        if ca.enabled {
            issues.push(ConfigIssue {
                severity: Severity::Ok,
                category: "Coding",
                message: format!(
                    "Coding agent: enabled, max_iterations={}, {} commands",
                    ca.max_iterations, ca.allowed_commands.len()
                ),
                suggestion: None,
            });
        }
    }
}

fn check_external_tools(issues: &mut Vec<ConfigIssue>) {
    // Chrome
    let chrome_ok = which("chrome")
        || Path::new("C:/Program Files/Google/Chrome/Application/chrome.exe").exists()
        || Path::new("C:/Program Files (x86)/Google/Chrome/Application/chrome.exe").exists();
    issues.push(ConfigIssue {
        severity: if chrome_ok { Severity::Ok } else { Severity::Info },
        category: "Tools",
        message: if chrome_ok { "Chrome: found".into() } else { "Chrome: not found (browser features unavailable)".into() },
        suggestion: if chrome_ok { None } else { Some("Install Google Chrome for browser automation.".into()) },
    });

    // Claude CLI
    let claude_ok = crate::claude_session::cli_available();
    issues.push(ConfigIssue {
        severity: if claude_ok { Severity::Ok } else { Severity::Info },
        category: "Tools",
        message: if claude_ok {
            format!("Claude CLI: {}", crate::claude_session::cli_version().unwrap_or_default())
        } else {
            "Claude CLI: not found (cross-repo bug fixing unavailable)".into()
        },
        suggestion: if claude_ok { None } else { Some("Install Claude Code: npm install -g @anthropic-ai/claude-code".into()) },
    });
}

// ── Formatting ───────────────────────────────────────────────────────────────

pub fn format_report(issues: &[ConfigIssue]) -> String {
    let mut out = String::from("=== Sirin Config Check ===\n\n");
    let errors = issues.iter().filter(|i| i.severity == Severity::Error).count();
    let warnings = issues.iter().filter(|i| i.severity == Severity::Warning).count();
    let oks = issues.iter().filter(|i| i.severity == Severity::Ok).count();

    out.push_str(&format!("Summary: {oks} OK, {warnings} warnings, {errors} errors\n\n"));

    for issue in issues {
        out.push_str(&format!(
            "{} [{}] {}\n",
            issue.severity.icon(),
            issue.category,
            issue.message
        ));
        if let Some(s) = &issue.suggestion {
            out.push_str(&format!("     -> {s}\n"));
        }
    }
    out
}

/// Log issues to stderr (startup use).  Only prints if there are warnings or errors.
pub fn log_startup(issues: &[ConfigIssue]) {
    let has_problems = issues.iter().any(|i| matches!(i.severity, Severity::Warning | Severity::Error));
    if !has_problems { return; }

    eprintln!("[config] Diagnostics:");
    for issue in issues.iter().filter(|i| !matches!(i.severity, Severity::Ok)) {
        eprintln!("  {} [{}] {}", issue.severity.icon(), issue.category, issue.message);
        if let Some(s) = &issue.suggestion {
            eprintln!("       -> {s}");
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
//  AI-ASSISTED FIXING
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ConfigFix {
    pub file: String,
    pub field_path: String,
    pub current_value: String,
    pub new_value: String,
    pub reason: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AiAdvice {
    pub analysis: String,
    #[serde(default)]
    pub proposed_fixes: Vec<ConfigFix>,
}

/// Ask the main LLM to analyze diagnostic issues and propose fixes.
/// Returns structured advice — does NOT modify any files.
pub async fn ai_analyze() -> Result<AiAdvice, String> {
    let issues = run_diagnostics();
    let issues_md = format_report(&issues);

    let llm_yaml = std::fs::read_to_string("config/llm.yaml")
        .unwrap_or_else(|_| "# (missing)".into());
    let persona_yaml = std::fs::read_to_string("config/persona.yaml")
        .unwrap_or_else(|_| "# (missing)".into());
    let env_provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
    let env_gemini_model = std::env::var("GEMINI_MODEL").unwrap_or_default();
    let env_router_provider = std::env::var("ROUTER_LLM_PROVIDER").unwrap_or_default();

    let prompt = format!(
        r#"You are a Sirin configuration advisor. Analyze the diagnostic issues and current config files below, then output STRICTLY valid JSON matching this schema:

{{
  "analysis": "<Markdown summary in 繁體中文 explaining the overall config health>",
  "proposed_fixes": [
    {{
      "file": "config/llm.yaml",
      "field_path": "coding_model",
      "current_value": "",
      "new_value": "models/gemini-2.5-flash",
      "reason": "<why this helps, in 繁體中文>"
    }}
  ]
}}

RULES:
- Output ONLY the JSON object, no markdown code fences, no prose before/after.
- Only propose changes to: config/llm.yaml, config/persona.yaml.
- Each fix is a SINGLE field change.
- field_path uses dot notation (e.g. "roi_thresholds.min_usd_to_notify").
- Skip fixes that would duplicate existing values.
- If no fixes are needed, return empty proposed_fixes array.
- Prefer low-risk suggestions. Don't change API keys or security settings.

## Diagnostic Issues

{issues_md}

## Current config/llm.yaml

```yaml
{llm_yaml}
```

## Current config/persona.yaml

```yaml
{persona_yaml}
```

## Environment (read-only, for context)

- LLM_PROVIDER: {env_provider}
- GEMINI_MODEL: {env_gemini_model}
- ROUTER_LLM_PROVIDER: {env_router_provider}
"#
    );

    let client = crate::llm::shared_http();
    let llm = crate::llm::shared_llm();
    let raw = crate::llm::call_prompt(&client, &llm, prompt)
        .await
        .map_err(|e| format!("LLM call failed: {e}"))?;

    // Strip markdown fences if the model added them despite instructions.
    let json_str = extract_json(&raw);

    serde_json::from_str::<AiAdvice>(&json_str)
        .map_err(|e| format!("Failed to parse AI response as JSON: {e}\n\nRaw response:\n{raw}"))
}

/// Apply approved fixes.  Backs up each file to `.bak.TIMESTAMP` before editing.
/// Returns list of applied fix descriptions.
pub fn apply_fixes(fixes: &[ConfigFix]) -> Result<Vec<String>, String> {
    let mut applied = Vec::new();
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();

    // Group fixes by file so each file gets a single backup + edit pass.
    use std::collections::HashMap;
    let mut by_file: HashMap<&str, Vec<&ConfigFix>> = HashMap::new();
    for f in fixes {
        if !ALLOWED_FILES.contains(&f.file.as_str()) {
            return Err(format!("File not in allowlist: {}", f.file));
        }
        by_file.entry(f.file.as_str()).or_default().push(f);
    }

    for (file, file_fixes) in by_file {
        // Backup
        let backup_path = format!("{file}.bak.{ts}");
        std::fs::copy(file, &backup_path)
            .map_err(|e| format!("Failed to backup {file}: {e}"))?;

        // Read → mutate → write
        let content = std::fs::read_to_string(file)
            .map_err(|e| format!("Failed to read {file}: {e}"))?;
        let mut yaml_value: serde_yaml::Value = serde_yaml::from_str(&content)
            .map_err(|e| format!("Failed to parse {file}: {e}"))?;

        for fix in &file_fixes {
            set_field_by_path(&mut yaml_value, &fix.field_path, &fix.new_value)
                .map_err(|e| format!("Fix {}::{}: {e}", fix.file, fix.field_path))?;
        }

        let new_content = serde_yaml::to_string(&yaml_value)
            .map_err(|e| format!("Failed to serialize {file}: {e}"))?;
        std::fs::write(file, new_content)
            .map_err(|e| format!("Failed to write {file}: {e}"))?;

        for fix in &file_fixes {
            applied.push(format!("{}::{} = {}", fix.file, fix.field_path, fix.new_value));
        }
    }

    Ok(applied)
}

/// Extract JSON from a response, stripping markdown fences if present.
fn extract_json(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip ```json ... ``` or ``` ... ```
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let after = after.trim_start_matches('\n');
        if let Some(end) = after.rfind("```") {
            return after[..end].trim().to_string();
        }
    }
    // Find first { and last }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if end > start {
            return trimmed[start..=end].to_string();
        }
    }
    trimmed.to_string()
}

/// Set a YAML value by dot-notation path.  Creates intermediate maps as needed.
fn set_field_by_path(root: &mut serde_yaml::Value, path: &str, new_value: &str) -> Result<(), String> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return Err("empty field_path".into());
    }

    // Navigate/create nested structure
    let mut current = root;
    for part in &parts[..parts.len() - 1] {
        if !current.is_mapping() {
            *current = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        }
        let map = current.as_mapping_mut().unwrap();
        let key = serde_yaml::Value::String(part.to_string());
        if !map.contains_key(&key) {
            map.insert(key.clone(), serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        }
        current = map.get_mut(&key).unwrap();
    }

    if !current.is_mapping() {
        *current = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let last = parts.last().unwrap();
    let map = current.as_mapping_mut().unwrap();
    // Try to parse new_value as number/bool/null, else keep as string.
    let parsed: serde_yaml::Value = serde_yaml::from_str(new_value)
        .unwrap_or_else(|_| serde_yaml::Value::String(new_value.to_string()));
    map.insert(serde_yaml::Value::String(last.to_string()), parsed);
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn url_to_addr(url: &str) -> Option<std::net::SocketAddr> {
    let url = url.trim_start_matches("http://").trim_start_matches("https://");
    let url = url.trim_end_matches('/');
    // Split host:port
    if let Some((host, port)) = url.rsplit_once(':') {
        let host = host.split('/').next().unwrap_or(host);
        let port: u16 = port.split('/').next()?.parse().ok()?;
        use std::net::ToSocketAddrs;
        format!("{host}:{port}").to_socket_addrs().ok()?.next()
    } else {
        None
    }
}

fn which(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_returns_nonempty() {
        let issues = run_diagnostics();
        assert!(!issues.is_empty(), "should have at least config file checks");
    }

    #[test]
    fn format_report_includes_summary() {
        let issues = vec![
            ConfigIssue { severity: Severity::Ok, category: "Test", message: "good".into(), suggestion: None },
            ConfigIssue { severity: Severity::Warning, category: "Test", message: "warn".into(), suggestion: Some("fix it".into()) },
        ];
        let report = format_report(&issues);
        assert!(report.contains("1 OK"));
        assert!(report.contains("1 warnings"));
        assert!(report.contains("fix it"));
    }

    #[test]
    fn severity_ordering() {
        assert!(Severity::Error > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
        assert!(Severity::Info > Severity::Ok);
    }

    /// Print the actual diagnostic report for this workspace.
    #[test]
    #[ignore]
    fn print_diagnostics() {
        let _ = dotenvy::dotenv();
        let issues = run_diagnostics();
        let report = format_report(&issues);
        println!("\n{report}");
    }

    #[test]
    fn apply_fixes_rejects_files_outside_allowlist() {
        let fix = ConfigFix {
            file: "config/hackme.yaml".into(),  // NOT in allowlist
            field_path: "foo".into(),
            current_value: "".into(),
            new_value: "bar".into(),
            reason: "evil".into(),
        };
        let result = apply_fixes(&[fix]);
        assert!(result.is_err(), "should reject non-allowlisted file");
        assert!(result.unwrap_err().contains("allowlist"));
    }

    #[test]
    fn apply_fixes_rejects_env_file() {
        let fix = ConfigFix {
            file: ".env".into(),
            field_path: "GEMINI_API_KEY".into(),
            current_value: "".into(),
            new_value: "leaked".into(),
            reason: "attack".into(),
        };
        let result = apply_fixes(&[fix]);
        assert!(result.is_err(), "should reject .env file");
    }

    #[test]
    fn set_field_by_path_nested() {
        let mut root = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        set_field_by_path(&mut root, "a.b.c", "42").expect("set");
        let yaml = serde_yaml::to_string(&root).unwrap();
        assert!(yaml.contains("a:"));
        assert!(yaml.contains("b:"));
        assert!(yaml.contains("c: 42"));
    }

    #[test]
    fn set_field_by_path_preserves_existing() {
        let mut root: serde_yaml::Value = serde_yaml::from_str("x: 1\ny: 2").unwrap();
        set_field_by_path(&mut root, "z", "3").expect("set");
        let yaml = serde_yaml::to_string(&root).unwrap();
        assert!(yaml.contains("x: 1"));
        assert!(yaml.contains("y: 2"));
        assert!(yaml.contains("z: 3"));
    }

    #[test]
    fn extract_json_handles_markdown_fences() {
        let raw = "```json\n{\"analysis\":\"ok\",\"proposed_fixes\":[]}\n```";
        let json = extract_json(raw);
        assert!(json.contains("analysis"));
        assert!(!json.contains("```"));
    }

    #[test]
    fn extract_json_handles_bare_json() {
        let raw = "Here is the result: {\"a\":1}";
        let json = extract_json(raw);
        assert_eq!(json, "{\"a\":1}");
    }

    /// E2E: call the real LLM to analyze config.  Needs GEMINI_API_KEY.
    /// Run: cargo test --bin sirin test_ai_analyze -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_ai_analyze() {
        let _ = dotenvy::dotenv();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let advice = rt.block_on(ai_analyze()).expect("ai_analyze failed");

        println!("\n=== AI Analysis ===\n{}\n", advice.analysis);
        println!("=== Proposed Fixes ({}) ===", advice.proposed_fixes.len());
        for (i, fix) in advice.proposed_fixes.iter().enumerate() {
            println!("\n[{}] {} :: {}", i + 1, fix.file, fix.field_path);
            println!("    current: {:?}", fix.current_value);
            println!("    new:     {:?}", fix.new_value);
            println!("    reason:  {}", fix.reason);
        }

        // Basic sanity
        assert!(!advice.analysis.is_empty(), "analysis should not be empty");
        for fix in &advice.proposed_fixes {
            assert!(
                fix.file == "config/llm.yaml" || fix.file == "config/persona.yaml",
                "AI proposed write to disallowed file: {}",
                fix.file
            );
        }
    }
}
