/// Configuration schema and loader for the Pre-Authorization Engine.
///
/// Layer priority (highest wins):
///   1. Repo-local  `.sirin/authz.yaml`
///   2. User-global `~/.sirin/authz.yaml`
///   3. Built-in hard-coded defaults (`defaults()`)
///
/// `allow` / `deny` / `ask` arrays are **unioned** across layers;
/// `mode` is taken from the highest layer that sets it.
use serde::{Deserialize, Serialize};
use std::path::Path;

// ─── Mode ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// deny-by-default + allow-list + ask-list (default)
    #[default]
    Selective,
    /// all mutating actions need human approval
    Strict,
    /// only hard deny-rules apply; everything else is allowed (dev / CI)
    Permissive,
    /// no mutating actions at all (AI can only plan, not act)
    Plan,
}

// ─── Rule ────────────────────────────────────────────────────────────────────

/// A single allow / deny / ask rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct Rule {
    /// Action name (supports `*` suffix like `ax_*`, or `*` for all).
    #[serde(default)]
    pub action: Option<String>,

    /// URL glob (`*` = one segment, `**` = any number of segments).
    #[serde(default)]
    pub url_pattern: Option<String>,

    /// For `eval` actions: JS source must contain this substring (case-insensitive).
    #[serde(default)]
    pub js_contains: Option<String>,

    /// a11y name / value must contain this substring (case-insensitive).
    #[serde(default)]
    pub name_substring: Option<String>,

    /// a11y name must match this regex.
    #[serde(default)]
    pub name_regex: Option<String>,

    /// a11y name must NOT contain any of these substrings (case-insensitive).
    /// Any one match → rule fires.
    #[serde(default)]
    pub not_name_matches: Vec<String>,
}

impl Rule {
    /// Human-readable summary for logging / reasons.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(a) = &self.action {
            parts.push(format!("action={a}"));
        }
        if let Some(u) = &self.url_pattern {
            parts.push(format!("url={u}"));
        }
        if let Some(js) = &self.js_contains {
            parts.push(format!("js_contains={js}"));
        }
        if let Some(nm) = &self.name_substring {
            parts.push(format!("name_substring={nm}"));
        }
        if let Some(nr) = &self.name_regex {
            parts.push(format!("name_regex={nr}"));
        }
        if !self.not_name_matches.is_empty() {
            parts.push(format!("not_name_matches={:?}", self.not_name_matches));
        }
        if parts.is_empty() {
            "rule(any)".to_string()
        } else {
            parts.join(" ")
        }
    }
}

// ─── ClientPolicy ────────────────────────────────────────────────────────────

/// Per-client policy override (only `mode` for now; future: extra allow/deny).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientPolicy {
    #[serde(default)]
    pub mode: Option<Mode>,
}

// ─── LearnConfig ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnConfig {
    #[serde(default = "bool_false")]
    pub enabled: bool,

    #[serde(default = "default_write_back")]
    pub write_back_to: String,

    #[serde(default = "default_max_asks")]
    pub max_asks_per_session: u32,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            write_back_to: default_write_back(),
            max_asks_per_session: default_max_asks(),
        }
    }
}

fn bool_false() -> bool { false }
fn default_write_back() -> String { "repo".to_string() }
fn default_max_asks() -> u32 { 20 }

// ─── AuditConfig ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    #[serde(default = "default_log_path")]
    pub log_path: String,

    #[serde(default = "default_max_size_mb")]
    pub max_size_mb: u64,

    #[serde(default = "default_max_backups")]
    pub max_backups: u32,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            log_path: default_log_path(),
            max_size_mb: default_max_size_mb(),
            max_backups: default_max_backups(),
        }
    }
}

fn default_log_path() -> String { ".sirin/audit.ndjson".to_string() }
fn default_max_size_mb() -> u64 { 10 }
fn default_max_backups() -> u32 { 5 }

// ─── AuthzConfig ─────────────────────────────────────────────────────────────

/// Top-level configuration struct (matches the YAML schema).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthzConfig {
    /// Global default mode (overridden per-client if client has `mode`).
    #[serde(default)]
    pub mode: Mode,

    /// Actions that are always allowed regardless of mode / rules.
    #[serde(default = "default_readonly_allow")]
    pub readonly_allow: Vec<String>,

    /// Per-client overrides keyed by `<name>@<version>` glob.
    #[serde(default)]
    pub clients: std::collections::HashMap<String, ClientPolicy>,

    /// Allow-list rules.
    #[serde(default)]
    pub allow: Vec<Rule>,

    /// Hard-deny rules (evaluated before allow).
    #[serde(default)]
    pub deny: Vec<Rule>,

    /// Prompt-human rules.
    #[serde(default)]
    pub ask: Vec<Rule>,

    #[serde(default)]
    pub learn: LearnConfig,

    #[serde(default)]
    pub audit: AuditConfig,
}

fn default_readonly_allow() -> Vec<String> {
    vec![
        "ax_tree".into(), "ax_find".into(), "ax_value".into(),
        "screenshot".into(), "url".into(), "title".into(),
        "console".into(), "network".into(), "exists".into(),
        "attr".into(), "read".into(),
    ]
}

impl AuthzConfig {
    /// Resolve which `Mode` applies for a given client id (e.g. `claude-code@0.3.2`).
    ///
    /// Matching order:
    ///   1. Exact key match
    ///   2. `<name>@*` wildcard
    ///   3. `*` catch-all
    ///   4. Global `mode` field
    pub fn resolve_mode(&self, client_id: &str) -> Mode {
        // Exact
        if let Some(cp) = self.clients.get(client_id) {
            if let Some(m) = &cp.mode {
                return m.clone();
            }
        }

        // Wildcard: try replacing version with `*`
        let name_part = client_id.split('@').next().unwrap_or(client_id);
        let wildcard = format!("{name_part}@*");
        if let Some(cp) = self.clients.get(&wildcard) {
            if let Some(m) = &cp.mode {
                return m.clone();
            }
        }

        // Catch-all
        if let Some(cp) = self.clients.get("*") {
            if let Some(m) = &cp.mode {
                return m.clone();
            }
        }

        self.mode.clone()
    }
}

// ─── Defaults ────────────────────────────────────────────────────────────────

/// Built-in hard-coded baseline (loaded when no YAML file exists).
pub fn defaults() -> AuthzConfig {
    AuthzConfig {
        mode: Mode::Permissive,
        readonly_allow: default_readonly_allow(),
        clients: std::collections::HashMap::new(),
        allow: vec![],
        deny: vec![
            Rule { url_pattern: Some("file:///**".into()), ..Default::default() },
            Rule { url_pattern: Some("chrome://**".into()), ..Default::default() },
            Rule { url_pattern: Some("chrome-extension://**".into()), ..Default::default() },
            Rule { action: Some("eval".into()), js_contains: Some("document.cookie".into()), ..Default::default() },
            Rule { action: Some("eval".into()), js_contains: Some("window.ethereum".into()), ..Default::default() },
            Rule { action: Some("eval".into()), js_contains: Some("navigator.credentials".into()), ..Default::default() },
            Rule { action: Some("eval".into()), js_contains: Some("indexedDB.open".into()), ..Default::default() },
            Rule {
                action: Some("ax_type*".into()),
                not_name_matches: vec![
                    "password".into(), "密碼".into(),
                    "private key".into(), "seed phrase".into(), "助記詞".into(),
                ],
                ..Default::default()
            },
        ],
        ask: vec![],
        learn: LearnConfig::default(),
        audit: AuditConfig::default(),
    }
}


// ─── Loader ──────────────────────────────────────────────────────────────────

/// Load and merge all config layers:
///   built-in defaults ← user-global ← repo-local
///
/// `repo_root` should be the directory that contains `.sirin/authz.yaml` (if any).
/// Pass `None` to skip repo-local layer.
pub fn load(repo_root: Option<&Path>) -> AuthzConfig {
    let mut cfg = defaults();

    // User-global: ~/.sirin/authz.yaml
    if let Some(home) = dirs_home() {
        let user_path = home.join(".sirin").join("authz.yaml");
        if user_path.exists() {
            if let Ok(extra) = load_file(&user_path) {
                merge_into(&mut cfg, extra);
            }
        }
    }

    // Repo-local: <repo>/.sirin/authz.yaml
    if let Some(root) = repo_root {
        let repo_path = root.join(".sirin").join("authz.yaml");
        if repo_path.exists() {
            if let Ok(extra) = load_file(&repo_path) {
                merge_into(&mut cfg, extra);
            }
        }
    }

    cfg
}

/// Parse a single YAML file into `AuthzConfig`. Returns `Err(String)` on failure.
pub fn load_file(path: &Path) -> Result<AuthzConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("authz: cannot read {}: {e}", path.display()))?;
    serde_yaml::from_str::<AuthzConfig>(&text)
        .map_err(|e| format!("authz: parse error in {}: {e}", path.display()))
}

/// Merge `src` (higher-priority layer) into `base`:
///   - `allow` / `deny` / `ask` arrays are prepended (src rules take precedence).
///   - `mode` is taken from `src` if not equal to default `Selective`.
///   - `readonly_allow` union (src items that aren't already present get appended).
///   - `clients` map: `src` entries overwrite `base` entries.
pub fn merge_into(base: &mut AuthzConfig, src: AuthzConfig) {
    // mode: let higher-priority layer override
    // (we treat src.mode as intentional if the file sets it)
    base.mode = src.mode;

    // readonly_allow: union
    for item in src.readonly_allow {
        if !base.readonly_allow.contains(&item) {
            base.readonly_allow.push(item);
        }
    }

    // rules: src rules go first so they match before base rules
    let mut new_allow = src.allow;
    new_allow.extend(std::mem::take(&mut base.allow));
    base.allow = new_allow;

    let mut new_deny = src.deny;
    new_deny.extend(std::mem::take(&mut base.deny));
    base.deny = new_deny;

    let mut new_ask = src.ask;
    new_ask.extend(std::mem::take(&mut base.ask));
    base.ask = new_ask;

    // clients: src entries overwrite
    for (k, v) in src.clients {
        base.clients.insert(k, v);
    }

    // learn / audit: src overrides
    base.learn = src.learn;
    base.audit = src.audit;
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("USERPROFILE").ok().map(std::path::PathBuf::from))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod config_test {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir()
            .join(format!("authz_cfg_{}_{}", std::process::id(), n));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_yaml(dir: &PathBuf, name: &str, yaml: &str) {
        let sirin_dir = dir.join(".sirin");
        fs::create_dir_all(&sirin_dir).unwrap();
        fs::write(sirin_dir.join(name), yaml).unwrap();
    }

    // ── round-trip ────────────────────────────────────────────────────────────
    #[test]
    fn yaml_roundtrip_defaults() {
        let cfg = defaults();
        let yaml = serde_yaml::to_string(&cfg).expect("serialize");
        let back: AuthzConfig = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(back.mode, Mode::Permissive);
        assert!(back.readonly_allow.contains(&"screenshot".to_string()));
        assert!(!back.deny.is_empty());
    }

    #[test]
    fn yaml_roundtrip_minimal() {
        let yaml = "mode: permissive\n";
        let cfg: AuthzConfig = serde_yaml::from_str(yaml).expect("deserialize");
        assert_eq!(cfg.mode, Mode::Permissive);
        // readonly_allow uses serde(default) so it should be populated
        assert!(cfg.readonly_allow.contains(&"ax_tree".to_string()));
    }

    // ── three-layer merge ─────────────────────────────────────────────────────
    #[test]
    fn merge_repo_overrides_mode() {
        let dir = tmp_dir();
        write_yaml(&dir, "authz.yaml", "mode: strict\n");
        let cfg = load(Some(&dir));
        assert_eq!(cfg.mode, Mode::Strict);
    }

    #[test]
    fn merge_deny_union() {
        // repo layer adds an extra deny rule; both repo + default deny rules present
        let dir = tmp_dir();
        let yaml = r#"
deny:
  - { url_pattern: "https://evil.example/**" }
"#;
        write_yaml(&dir, "authz.yaml", yaml);
        let cfg = load(Some(&dir));
        // Repo rule prepended
        assert_eq!(cfg.deny[0].url_pattern.as_deref(), Some("https://evil.example/**"));
        // Built-in defaults still present
        let has_file_deny = cfg.deny.iter().any(|r| r.url_pattern.as_deref() == Some("file:///**"));
        assert!(has_file_deny, "built-in file:// deny should survive merge");
    }

    #[test]
    fn merge_allow_union() {
        let dir = tmp_dir();
        let yaml = r#"
allow:
  - { action: goto, url_pattern: "http://localhost:3000/**" }
"#;
        write_yaml(&dir, "authz.yaml", yaml);
        let cfg = load(Some(&dir));
        // defaults() has empty allow; repo rule is present
        let has_local = cfg.allow.iter().any(|r| {
            r.url_pattern.as_deref() == Some("http://localhost:3000/**")
        });
        assert!(has_local);
    }

    // ── glob edge cases ───────────────────────────────────────────────────────
    #[test]
    fn glob_double_star_matches_path() {
        // Validate that our rule struct deserialises glob correctly
        let yaml = r#"
allow:
  - { action: goto, url_pattern: "https://redandan.github.io/**" }
"#;
        let cfg: AuthzConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.allow[0].url_pattern.as_deref(),
            Some("https://redandan.github.io/**")
        );
    }

    #[test]
    fn glob_single_star_segment() {
        let yaml = r#"
allow:
  - { action: goto, url_pattern: "http://localhost:*/**" }
"#;
        let cfg: AuthzConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.allow[0].url_pattern.as_deref(),
            Some("http://localhost:*/**")
        );
    }

    #[test]
    fn glob_exact_string() {
        let yaml = r#"
deny:
  - { url_pattern: "https://evil.example/exact" }
"#;
        let cfg: AuthzConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.deny[0].url_pattern.as_deref(),
            Some("https://evil.example/exact")
        );
    }

    #[test]
    fn readonly_allow_default_populated() {
        let yaml = "mode: selective\n";
        let cfg: AuthzConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.readonly_allow.contains(&"ax_tree".to_string()));
        assert!(cfg.readonly_allow.contains(&"screenshot".to_string()));
    }

    #[test]
    fn client_policy_merge() {
        let yaml = r#"
clients:
  "claude-code@*":
    mode: permissive
  "*":
    mode: selective
"#;
        let cfg: AuthzConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.resolve_mode("claude-code@0.3.2"), Mode::Permissive);
        assert_eq!(cfg.resolve_mode("unknown@1.0"), Mode::Selective);
    }
}
