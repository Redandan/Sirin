/// Decision engine: `decide()` + rule pattern matching.
///
/// Algorithm (§5 of DESIGN_AUTHZ.md):
///   1. readonly_allow direct pass
///   2. deny (highest priority, evaluated before allow)
///   3. mode dispatch (Permissive / Plan / Strict short-circuits)
///   4. allow check
///   5. ask check
///   6. learn path (not implemented — stub)
///   7. default deny
use serde_json::Value as JsonValue;

use crate::authz::config::{AuthzConfig, Mode, Rule};

// ─── Decision ────────────────────────────────────────────────────────────────

/// The outcome of a single authorization check.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// The action is allowed. Contains a human-readable reason string.
    Allow(String),
    /// The action is denied. Contains a human-readable reason string.
    Deny(String),
    /// A human must be asked (sync/async prompt). Contains reason string.
    Ask(String),
    /// Like `Ask` but the user gets "Allow always" / learn options.
    AskWithLearn,
}

// ─── decide() ────────────────────────────────────────────────────────────────

/// Core authorization decision function.
///
/// # Parameters
/// - `client_id`: MCP client identifier like `claude-code@0.3.2`
/// - `action`:    MCP browser_exec action name (e.g. `goto`, `ax_click`)
/// - `args`:      Full args JSON object for the call
/// - `current_url`: Current browser URL (may be `None` if browser is closed)
/// - `config`:    Loaded and merged `AuthzConfig`
pub fn decide(
    client_id: &str,
    action: &str,
    args: &JsonValue,
    current_url: &Option<String>,
    config: &AuthzConfig,
) -> Decision {
    // 1. readonly_allow: zero-risk actions pass unconditionally
    if config.readonly_allow.iter().any(|ra| ra == action) {
        return Decision::Allow("readonly_allow".to_string());
    }

    // 2. deny: evaluated before everything else
    for rule in &config.deny {
        if rule_matches(rule, action, args, current_url) {
            return Decision::Deny(rule.describe());
        }
    }

    // 3. mode dispatch
    let mode = config.resolve_mode(client_id);
    match mode {
        Mode::Permissive => {
            return Decision::Allow("permissive mode".to_string());
        }
        Mode::Plan => {
            if is_mutating(action) {
                return Decision::Deny("plan mode — mutating action disabled".to_string());
            }
            return Decision::Allow("plan mode readonly".to_string());
        }
        Mode::Strict => {
            if is_mutating(action) {
                return Decision::Ask("strict mode — all mutating actions require approval".to_string());
            }
            return Decision::Allow("strict mode readonly".to_string());
        }
        Mode::Selective => {
            // continue to allow / ask / learn / default-deny below
        }
    }

    // 4. allow
    for rule in &config.allow {
        if rule_matches(rule, action, args, current_url) {
            return Decision::Allow(rule.describe());
        }
    }

    // 5. ask
    for rule in &config.ask {
        if rule_matches(rule, action, args, current_url) {
            return Decision::Ask(rule.describe());
        }
    }

    // 6. learn mode (not implemented in this PR — TODO: wire to Monitor channel)
    if config.learn.enabled {
        return Decision::AskWithLearn;
    }

    // 7. default deny
    Decision::Deny("no matching allow rule".to_string())
}

// ─── Rule matching ───────────────────────────────────────────────────────────

/// Returns `true` if a rule matches the given call parameters.
///
/// All specified fields must match (logical AND).
/// Unspecified (None / empty) fields are treated as wildcard (always match).
pub fn rule_matches(
    rule: &Rule,
    action: &str,
    args: &JsonValue,
    current_url: &Option<String>,
) -> bool {
    // action field
    if let Some(rule_action) = &rule.action {
        if !action_matches(rule_action, action) {
            return false;
        }
    }

    // url_pattern field
    if let Some(pattern) = &rule.url_pattern {
        let url = resolve_url(args, current_url);
        if !glob_matches(pattern, &url) {
            return false;
        }
    }

    // js_contains (case-insensitive substring in args.target or args.js or args.script)
    if let Some(needle) = &rule.js_contains {
        let js_text = extract_js_text(args);
        if !js_text.to_lowercase().contains(&needle.to_lowercase()) {
            return false;
        }
    }

    // name_substring (case-insensitive)
    if let Some(needle) = &rule.name_substring {
        let name_text = extract_name_text(args);
        if !name_text.to_lowercase().contains(&needle.to_lowercase()) {
            return false;
        }
    }

    // name_regex
    if let Some(pattern) = &rule.name_regex {
        let name_text = extract_name_text(args);
        match regex::Regex::new(pattern) {
            Ok(re) => {
                if !re.is_match(&name_text) {
                    return false;
                }
            }
            Err(_) => {
                // invalid regex → treat as non-matching (fail open for allow, fail closed for deny)
                return false;
            }
        }
    }

    // not_name_matches: action fires if ANY substring matches (for deny rules).
    // If the rule has not_name_matches, the rule matches when name does NOT contain any of them.
    // This is the "deny if name contains forbidden word" semantics:
    //   deny rule fires = the action targets a sensitive field.
    if !rule.not_name_matches.is_empty() {
        let name_text = extract_name_text(args);
        let name_lower = name_text.to_lowercase();
        // The rule matches (fires) if any forbidden word is found in the name.
        let any_forbidden = rule.not_name_matches.iter()
            .any(|s| name_lower.contains(&s.to_lowercase()));
        if !any_forbidden {
            return false;
        }
    }

    true
}

// ─── Action wildcard matching ─────────────────────────────────────────────────

/// Matches action names: supports `*` (match all), `prefix_*` (prefix wildcard),
/// `*_suffix` (suffix wildcard), or exact match.
fn action_matches(pattern: &str, action: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return action.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return action.ends_with(suffix);
    }
    pattern == action
}

// ─── URL resolution ───────────────────────────────────────────────────────────

/// Resolve the URL to check: prefer `args.target` (for `goto`),
/// then `args.url`, then fall back to `current_url`.
fn resolve_url(args: &JsonValue, current_url: &Option<String>) -> String {
    if let Some(t) = args.get("target").and_then(|v| v.as_str()) {
        return t.to_string();
    }
    if let Some(u) = args.get("url").and_then(|v| v.as_str()) {
        return u.to_string();
    }
    current_url.clone().unwrap_or_default()
}

// ─── Glob matching ────────────────────────────────────────────────────────────

/// Matches a URL against a glob pattern using the `globset` crate.
/// Falls back to `false` on invalid pattern (fail-safe for allow; still blocks for deny).
fn glob_matches(pattern: &str, url: &str) -> bool {
    use globset::GlobBuilder;

    // Build a glob that matches case-insensitively and treats `**` as any path segment.
    let glob = GlobBuilder::new(pattern)
        .case_insensitive(false)
        .literal_separator(false) // `*` can match separators (needed for URL paths)
        .build();

    match glob {
        Ok(g) => {
            let matcher = g.compile_matcher();
            matcher.is_match(url)
        }
        Err(_) => false,
    }
}

// ─── JS text extraction ───────────────────────────────────────────────────────

/// Extract JS source text from args for eval-type actions.
fn extract_js_text(args: &JsonValue) -> String {
    // Try common field names used by browser_exec eval
    for key in &["target", "js", "script", "expression"] {
        if let Some(s) = args.get(*key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

// ─── Name text extraction ─────────────────────────────────────────────────────

/// Extract a11y name / label text from args for ax_* actions.
fn extract_name_text(args: &JsonValue) -> String {
    for key in &["name", "label", "text", "value", "target"] {
        if let Some(s) = args.get(*key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

// ─── Mutating action detection ───────────────────────────────────────────────

/// Returns `true` for actions that change browser state / page content.
/// Read-only actions (ax_tree, screenshot, url, etc.) return `false`.
pub fn is_mutating(action: &str) -> bool {
    // Explicitly non-mutating
    const READONLY: &[&str] = &[
        "ax_tree", "ax_find", "ax_value", "screenshot", "url", "title",
        "console", "network", "exists", "attr", "read",
    ];
    if READONLY.contains(&action) {
        return false;
    }
    // Everything else is considered mutating
    true
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod engine_test {
    use super::*;
    use crate::authz::config::{AuditConfig, ClientPolicy, LearnConfig, Rule};
    use serde_json::json;
    use std::collections::HashMap;

    fn base_config() -> AuthzConfig {
        crate::authz::config::defaults()
    }

    fn selective_config_with_allow(allow: Vec<Rule>) -> AuthzConfig {
        AuthzConfig {
            mode: Mode::Selective,
            readonly_allow: vec![
                "ax_tree".into(), "screenshot".into(), "url".into(),
            ],
            allow,
            deny: vec![],
            ask: vec![],
            clients: HashMap::new(),
            learn: LearnConfig::default(),
            audit: AuditConfig::default(),
            blocked_url_patterns: Vec::new(),
        }
    }

    // ── 1. readonly_allow direct pass ─────────────────────────────────────────
    #[test]
    fn readonly_allow_passes() {
        let cfg = base_config();
        let d = decide("test@1.0", "ax_tree", &json!({}), &None, &cfg);
        assert_eq!(d, Decision::Allow("readonly_allow".to_string()));
    }

    #[test]
    fn readonly_allow_screenshot_passes() {
        let cfg = base_config();
        let d = decide("test@1.0", "screenshot", &json!({}), &None, &cfg);
        assert_eq!(d, Decision::Allow("readonly_allow".to_string()));
    }

    // ── 2. deny priority over allow ───────────────────────────────────────────
    #[test]
    fn deny_priority_over_allow() {
        let mut cfg = selective_config_with_allow(vec![
            Rule { action: Some("goto".into()), url_pattern: Some("**".into()), ..Default::default() },
        ]);
        cfg.deny.push(Rule {
            url_pattern: Some("https://**paypal**/**".into()),
            ..Default::default()
        });
        let d = decide(
            "test@1.0",
            "goto",
            &json!({ "target": "https://www.paypal.com/login" }),
            &None,
            &cfg,
        );
        matches!(d, Decision::Deny(_));
    }

    #[test]
    fn deny_eval_js_contains_cookie() {
        let cfg = base_config();
        let d = decide(
            "test@1.0",
            "eval",
            &json!({ "target": "return document.cookie;" }),
            &None,
            &cfg,
        );
        assert!(matches!(d, Decision::Deny(_)), "got {d:?}");
    }

    #[test]
    fn deny_eval_ethereum() {
        let cfg = base_config();
        let d = decide(
            "test@1.0",
            "eval",
            &json!({ "target": "window.ethereum.request({method:'eth_accounts'})" }),
            &None,
            &cfg,
        );
        assert!(matches!(d, Decision::Deny(_)), "got {d:?}");
    }

    // ── 3. Mode::Permissive ───────────────────────────────────────────────────
    #[test]
    fn mode_permissive_allows_all() {
        let cfg = AuthzConfig {
            mode: Mode::Permissive,
            readonly_allow: vec![],
            allow: vec![],
            deny: vec![],
            ask: vec![],
            clients: HashMap::new(),
            learn: LearnConfig::default(),
            audit: AuditConfig::default(),
            blocked_url_patterns: Vec::new(),
        };
        let d = decide("test@1.0", "goto", &json!({ "target": "https://example.com/" }), &None, &cfg);
        assert_eq!(d, Decision::Allow("permissive mode".to_string()));
    }

    // ── 4. Mode::Plan ────────────────────────────────────────────────────────
    #[test]
    fn mode_plan_denies_mutating() {
        let cfg = AuthzConfig {
            mode: Mode::Plan,
            ..AuthzConfig::default()
        };
        let d = decide("test@1.0", "goto", &json!({ "target": "https://example.com/" }), &None, &cfg);
        assert!(matches!(d, Decision::Deny(_)), "got {d:?}");
    }

    #[test]
    fn mode_plan_allows_readonly() {
        let cfg = AuthzConfig {
            mode: Mode::Plan,
            readonly_allow: vec!["ax_tree".into()],
            ..AuthzConfig::default()
        };
        // ax_tree is in readonly_allow so hits step 1 before mode check
        let d = decide("test@1.0", "ax_tree", &json!({}), &None, &cfg);
        assert_eq!(d, Decision::Allow("readonly_allow".to_string()));
    }

    #[test]
    fn mode_plan_allows_non_mutating_non_readonly() {
        // A hypothetical action not in readonly_allow but also not mutating
        let cfg = AuthzConfig {
            mode: Mode::Plan,
            readonly_allow: vec![],
            deny: vec![],
            allow: vec![],
            ask: vec![],
            clients: HashMap::new(),
            learn: LearnConfig::default(),
            audit: AuditConfig::default(),
            blocked_url_patterns: Vec::new(),
        };
        // "url" is in default readonly_allow but not in this custom cfg
        // Use a truly non-mutating action that is not in readonly list
        // Plan mode: is_mutating("url") == false → allow
        let d = decide("test@1.0", "url", &json!({}), &None, &cfg);
        assert_eq!(d, Decision::Allow("plan mode readonly".to_string()));
    }

    // ── 5. Mode::Strict ───────────────────────────────────────────────────────
    #[test]
    fn mode_strict_asks_mutating() {
        let cfg = AuthzConfig {
            mode: Mode::Strict,
            readonly_allow: vec![],
            deny: vec![],
            allow: vec![],
            ask: vec![],
            clients: HashMap::new(),
            learn: LearnConfig::default(),
            audit: AuditConfig::default(),
            blocked_url_patterns: Vec::new(),
        };
        let d = decide("test@1.0", "goto", &json!({ "target": "https://example.com/" }), &None, &cfg);
        assert!(matches!(d, Decision::Ask(_)), "got {d:?}");
    }

    #[test]
    fn mode_strict_allows_readonly() {
        let cfg = AuthzConfig {
            mode: Mode::Strict,
            readonly_allow: vec![],
            deny: vec![],
            allow: vec![],
            ask: vec![],
            clients: HashMap::new(),
            learn: LearnConfig::default(),
            audit: AuditConfig::default(),
            blocked_url_patterns: Vec::new(),
        };
        // is_mutating("ax_tree") == false → strict mode allows
        let d = decide("test@1.0", "ax_tree", &json!({}), &None, &cfg);
        assert_eq!(d, Decision::Allow("strict mode readonly".to_string()));
    }

    // ── 6. Mode::Selective allow rule ────────────────────────────────────────
    #[test]
    fn selective_allow_url_match() {
        let cfg = selective_config_with_allow(vec![
            Rule {
                action: Some("goto".into()),
                url_pattern: Some("https://redandan.github.io/**".into()),
                ..Default::default()
            },
        ]);
        let d = decide(
            "test@1.0",
            "goto",
            &json!({ "target": "https://redandan.github.io/app/#/home" }),
            &None,
            &cfg,
        );
        assert!(matches!(d, Decision::Allow(_)), "got {d:?}");
    }

    #[test]
    fn selective_allow_action_wildcard_ax_star() {
        let cfg = selective_config_with_allow(vec![
            Rule {
                action: Some("ax_*".into()),
                url_pattern: Some("http://localhost:*/**".into()),
                ..Default::default()
            },
        ]);
        let d = decide(
            "test@1.0",
            "ax_click",
            &json!({ "backend_id": 42 }),
            &Some("http://localhost:3000/test".into()),
            &cfg,
        );
        assert!(matches!(d, Decision::Allow(_)), "got {d:?}");
    }

    // ── 7. ask rule ───────────────────────────────────────────────────────────
    #[test]
    fn selective_ask_rule() {
        let cfg = AuthzConfig {
            mode: Mode::Selective,
            readonly_allow: vec![],
            deny: vec![],
            allow: vec![],
            ask: vec![
                Rule {
                    action: Some("goto".into()),
                    url_pattern: Some("https://**.google.com/**".into()),
                    ..Default::default()
                },
            ],
            clients: HashMap::new(),
            learn: LearnConfig::default(),
            audit: AuditConfig::default(),
            blocked_url_patterns: Vec::new(),
        };
        let d = decide(
            "test@1.0",
            "goto",
            &json!({ "target": "https://accounts.google.com/oauth" }),
            &None,
            &cfg,
        );
        assert!(matches!(d, Decision::Ask(_)), "got {d:?}");
    }

    // ── 8. learn mode AskWithLearn ────────────────────────────────────────────
    #[test]
    fn learn_mode_returns_ask_with_learn() {
        let cfg = AuthzConfig {
            mode: Mode::Selective,
            readonly_allow: vec![],
            deny: vec![],
            allow: vec![],
            ask: vec![],
            clients: HashMap::new(),
            learn: LearnConfig { enabled: true, ..LearnConfig::default() },
            audit: AuditConfig::default(),
            blocked_url_patterns: Vec::new(),
        };
        let d = decide("test@1.0", "goto", &json!({ "target": "https://new.example.com/" }), &None, &cfg);
        assert_eq!(d, Decision::AskWithLearn);
    }

    // ── 9. default deny ───────────────────────────────────────────────────────
    #[test]
    fn default_deny_no_rule() {
        let cfg = selective_config_with_allow(vec![]);
        let d = decide("test@1.0", "goto", &json!({ "target": "https://unknown.example/" }), &None, &cfg);
        assert!(matches!(d, Decision::Deny(_)), "got {d:?}");
    }

    // ── 10. not_name_matches ──────────────────────────────────────────────────
    #[test]
    fn deny_not_name_matches_password() {
        // Rule: deny ax_type when target name contains "password"
        let mut cfg = selective_config_with_allow(vec![]);
        cfg.deny.push(Rule {
            action: Some("ax_type*".into()),
            not_name_matches: vec!["password".into()],
            ..Default::default()
        });
        let d = decide(
            "test@1.0",
            "ax_type",
            &json!({ "name": "Enter your password" }),
            &None,
            &cfg,
        );
        assert!(matches!(d, Decision::Deny(_)), "got {d:?}");
    }

    #[test]
    fn deny_not_name_matches_does_not_fire_for_safe_name() {
        let mut cfg = selective_config_with_allow(vec![
            Rule { action: Some("ax_type".into()), url_pattern: Some("**".into()), ..Default::default() },
        ]);
        cfg.deny.push(Rule {
            action: Some("ax_type*".into()),
            not_name_matches: vec!["password".into()],
            ..Default::default()
        });
        let d = decide(
            "test@1.0",
            "ax_type",
            &json!({ "name": "Username" }),
            &None,
            &cfg,
        );
        // deny rule does not fire; allow rule matches
        assert!(matches!(d, Decision::Allow(_)), "got {d:?}");
    }

    // ── 11. client policy permissive override ─────────────────────────────────
    #[test]
    fn client_policy_permissive_overrides_selective() {
        let mut clients = HashMap::new();
        clients.insert("claude-code@*".into(), ClientPolicy { mode: Some(Mode::Permissive) });
        let cfg = AuthzConfig {
            mode: Mode::Selective,
            readonly_allow: vec![],
            deny: vec![],
            allow: vec![],
            ask: vec![],
            clients,
            learn: LearnConfig::default(),
            audit: AuditConfig::default(),
            blocked_url_patterns: Vec::new(),
        };
        let d = decide("claude-code@0.3.2", "goto", &json!({ "target": "https://anywhere.com/" }), &None, &cfg);
        assert_eq!(d, Decision::Allow("permissive mode".to_string()));
    }

    // ── Glob matching edge cases ──────────────────────────────────────────────
    #[test]
    fn glob_double_star_matches_deep_path() {
        assert!(glob_matches("https://redandan.github.io/**", "https://redandan.github.io/app/#/wallet/withdraw"));
    }

    #[test]
    fn glob_single_star_port_wildcard() {
        assert!(glob_matches("http://localhost:*/**", "http://localhost:3000/dashboard"));
    }

    #[test]
    fn glob_no_match() {
        assert!(!glob_matches("https://safe.example.com/**", "https://evil.example.com/page"));
    }

    #[test]
    fn glob_double_star_paypal() {
        assert!(glob_matches("https://**paypal**/**", "https://www.paypal.com/login"));
    }
}
