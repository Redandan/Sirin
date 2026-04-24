//! YAML parsing for test goals.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A single browser action in a fixture step.
/// Mirrors the browser_exec action format.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FixtureStep {
    /// Action name — same values as browser_exec `action` param.
    pub action: String,
    /// Primary target (CSS selector, URL, JS expression, etc).
    #[serde(default)]
    pub target: String,
    /// Text to type (for `type` action).
    #[serde(default)]
    pub text: String,
    /// Timeout in ms (for `wait`, `wait_new_tab`, etc).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Extra browser_exec parameters not covered by the fields above.
    /// Examples: `role`, `name_regex` for `shadow_click`;
    /// `width`/`height` for `set_viewport`; `selector` for element actions.
    /// These are forwarded verbatim in the JSON args passed to web_navigate.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Setup and cleanup steps for a test goal.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Fixture {
    /// Steps run before the test loop. Failure aborts the test.
    #[serde(default)]
    pub setup: Vec<FixtureStep>,
    /// Steps run after the test (pass or fail). Failures are logged but do not affect test result.
    #[serde(default)]
    pub cleanup: Vec<FixtureStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGoal {
    pub id: String,
    pub name: String,
    pub url: String,
    pub goal: String,
    #[serde(default = "default_max_iter")]
    pub max_iterations: u32,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Number of JSON-parse retries before giving up.  Default 3.
    #[serde(default = "default_parse_retries")]
    pub retry_on_parse_error: u32,
    /// Locale for LLM prompts and responses.  Supported: "zh-TW" (default), "en", "zh-CN".
    #[serde(default = "default_locale")]
    pub locale: String,
    /// Extra query parameters merged into `url` before navigation.
    /// Primary use: Flutter Canvas apps need `flutter-web-renderer: html`
    /// to have a real DOM that selectors can work against.
    #[serde(default)]
    pub url_query: BTreeMap<String, String>,
    /// Override Chrome headless mode for this test.
    /// Flutter CanvasKit / WebGL content does NOT paint correctly in
    /// headless mode — set to `false` for such tests.  Default: honours
    /// `SIRIN_BROWSER_HEADLESS` env var (which itself defaults to true).
    #[serde(default)]
    pub browser_headless: Option<bool>,
    /// Override the LLM backend used by the ReAct executor for this test.
    /// Recognized values:
    /// - `"claude_cli"` / `"claude"` — spawn `claude -p` subprocess
    ///   (Max plan, no API key, much higher JSON-output reliability,
    ///   ~3-5s per call overhead)
    /// - any other value or `None` — use Sirin's main LLM config
    ///   (Gemini / LM Studio / Ollama / Anthropic HTTP API)
    ///
    /// Resolution order: this field → `TEST_RUNNER_LLM_BACKEND` env var
    /// → main LLM config.
    #[serde(default)]
    pub llm_backend: Option<String>,
    #[serde(default)]
    pub success_criteria: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional fixture: setup steps (before test) and cleanup steps (after).
    #[serde(default)]
    pub fixture: Option<Fixture>,
    /// Documents that MUST be read before running or interpreting this test.
    ///
    /// Shown in `list_tests` output and included in the `run_test_async` MCP
    /// response as a `docs_refs` field + warning so callers cannot miss them.
    /// Typical entries: test-account docs, acceptance-criteria files, E2E skill.
    ///
    /// Paths are relative to the repo root (or absolute).
    #[serde(default)]
    pub docs_refs: Vec<String>,
    /// How the ReAct loop should observe the page before each LLM turn.
    /// - `text`   — legacy: no screenshot, truncated text observations only
    /// - `vision` — always screenshot + vision LLM call
    /// - `auto`   — screenshot only when Flutter / canvas is detected at runtime
    ///
    /// Default `text` → zero behavioural change for existing tests.  Opt-in
    /// explicitly on Flutter / canvas pages where the AX tree is unreliable.
    #[serde(default)]
    pub perception: crate::perception::PerceptionMode,
}

impl TestGoal {
    /// Return the navigation URL with `url_query` appended as query string.
    /// If the URL already has a query string, the params are merged (TestGoal
    /// values win on collision).
    pub fn full_url(&self) -> String {
        if self.url_query.is_empty() {
            return self.url.clone();
        }
        let (base, existing) = match self.url.split_once('?') {
            Some((b, q)) => (b.to_string(), q.to_string()),
            None => (self.url.clone(), String::new()),
        };
        // Parse existing query into map (preserving order via Vec).
        let mut params: Vec<(String, String)> = existing
            .split('&')
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                p.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .or_else(|| Some((p.to_string(), String::new())))
            })
            .collect();
        // Override / append from url_query
        for (k, v) in &self.url_query {
            if let Some(existing_val) = params.iter_mut().find(|(ek, _)| ek == k) {
                existing_val.1 = v.clone();
            } else {
                params.push((k.clone(), v.clone()));
            }
        }
        let qs = params
            .iter()
            .map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") })
            .collect::<Vec<_>>()
            .join("&");
        format!("{base}?{qs}")
    }
}

/// Default iteration ceiling.  20 gives Flutter/SPA tests enough room for
/// multi-step flows without per-test YAML overrides.  Complex flows (checkout,
/// OAuth, multi-page wizards) should still set `max_iterations: 30` in YAML.
fn default_max_iter() -> u32 { 20 }
fn default_timeout() -> u64 { 120 }
fn default_parse_retries() -> u32 { 3 }
fn default_locale() -> String { "zh-TW".into() }

/// Directory containing YAML test definitions.
fn tests_dir() -> PathBuf {
    crate::platform::config_dir().join("tests")
}

/// Load all YAML tests from `config/tests/` and any subdirectories.
pub fn load_all() -> Vec<TestGoal> {
    let dir = tests_dir();
    if !dir.exists() { return Vec::new(); }
    let mut out = Vec::new();
    load_dir_recursive(&dir, &mut out);
    out
}

/// Recursively walk `dir`, loading every `.yaml` / `.yml` file found.
fn load_dir_recursive(dir: &std::path::Path, out: &mut Vec<TestGoal>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            load_dir_recursive(&p, out);
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" { continue; }
        match load_file(&p) {
            Ok(g) => out.push(g),
            Err(e) => tracing::warn!("Failed to load test {p:?}: {e}"),
        }
    }
}

/// Load a single test file.
pub fn load_file(path: &std::path::Path) -> Result<TestGoal, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("read {path:?}: {e}"))?;
    serde_yaml::from_str::<TestGoal>(&content)
        .map_err(|e| format!("parse {path:?}: {e}"))
}

/// Find a test by ID.
pub fn find(test_id: &str) -> Option<TestGoal> {
    load_all().into_iter().find(|g| g.id == test_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_yaml() {
        let yaml = r#"
id: smoke_home
name: "Home smoke test"
url: "https://example.com"
goal: "Load home page and see welcome text"
"#;
        let g: TestGoal = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(g.id, "smoke_home");
        assert_eq!(g.max_iterations, 20); // default bumped from 15→20 (#22-4)
        assert_eq!(g.timeout_secs, 120);
        assert!(g.tags.is_empty());
    }

    #[test]
    fn parse_full_yaml() {
        let yaml = r#"
id: login
name: "Login flow"
url: "https://app.example.com/login"
goal: "Sign in with test credentials"
max_iterations: 8
timeout_secs: 60
success_criteria:
  - "URL contains /dashboard"
  - "No console errors"
tags: [auth, smoke, critical]
"#;
        let g: TestGoal = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(g.max_iterations, 8);
        assert_eq!(g.success_criteria.len(), 2);
        assert_eq!(g.tags, vec!["auth".to_string(), "smoke".into(), "critical".into()]);
    }

    #[test]
    fn parse_yaml_with_fixture() {
        let yaml = r##"
id: fixture_test
name: "Test with fixture"
url: "https://example.com"
goal: "Do something"
fixture:
  setup:
    - action: goto
      target: "https://example.com/login"
    - action: type
      target: "#email"
      text: "test@example.com"
  cleanup:
    - action: eval
      target: "localStorage.clear()"
"##;
        let goal: TestGoal = serde_yaml::from_str(yaml).unwrap();
        let fixture = goal.fixture.unwrap();
        assert_eq!(fixture.setup.len(), 2);
        assert_eq!(fixture.setup[0].action, "goto");
        assert_eq!(fixture.cleanup.len(), 1);
        assert_eq!(fixture.cleanup[0].action, "eval");
    }

    #[test]
    fn missing_required_fields_fails() {
        let yaml = "id: x\n";
        let r: Result<TestGoal, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err());
    }

    #[test]
    fn full_url_without_query_params_unchanged() {
        let g: TestGoal = serde_yaml::from_str(
            "id: x\nname: y\nurl: https://example.com\ngoal: g",
        ).unwrap();
        assert_eq!(g.full_url(), "https://example.com");
    }

    #[test]
    fn full_url_appends_url_query() {
        let g: TestGoal = serde_yaml::from_str(r#"
id: flutter_test
name: "Flutter test"
url: "https://app.example.com/"
goal: "test it"
url_query:
  flutter-web-renderer: html
"#).unwrap();
        assert_eq!(g.full_url(), "https://app.example.com/?flutter-web-renderer=html");
    }

    #[test]
    fn parse_yaml_with_llm_backend() {
        let yaml = r#"
id: heavy_test
name: "Heavy LLM test"
url: "https://example.com"
goal: "do something complex"
llm_backend: claude_cli
"#;
        let g: TestGoal = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(g.llm_backend.as_deref(), Some("claude_cli"));
    }

    #[test]
    fn parse_yaml_without_llm_backend_defaults_to_none() {
        let yaml = r#"
id: regular_test
name: "Regular test"
url: "https://example.com"
goal: "do something"
"#;
        let g: TestGoal = serde_yaml::from_str(yaml).unwrap();
        assert!(g.llm_backend.is_none());
    }

    #[test]
    fn parse_yaml_with_docs_refs() {
        let yaml = r#"
id: agora_staking
name: "Staking test"
url: "https://example.com"
goal: "verify staking"
docs_refs:
  - AgoraMarket/.claude/skills/agora-market-e2e/SKILL.md
  - docs/acceptance/issue_34_pledge.md
"#;
        let g: TestGoal = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(g.docs_refs.len(), 2);
        assert!(g.docs_refs[0].contains("SKILL.md"));
        assert!(g.docs_refs[1].contains("issue_34"));
    }

    #[test]
    fn parse_yaml_without_docs_refs_defaults_to_empty() {
        let yaml = "id: x\nname: y\nurl: https://example.com\ngoal: g\n";
        let g: TestGoal = serde_yaml::from_str(yaml).unwrap();
        assert!(g.docs_refs.is_empty());
    }

    #[test]
    fn full_url_merges_with_existing_query() {
        let g: TestGoal = serde_yaml::from_str(r#"
id: x
name: y
url: "https://app.com/?foo=1&bar=2"
goal: g
url_query:
  flutter-web-renderer: html
  foo: OVERRIDE
"#).unwrap();
        let url = g.full_url();
        assert!(url.contains("foo=OVERRIDE"), "override should win: {url}");
        assert!(url.contains("bar=2"), "existing non-conflicting param kept: {url}");
        assert!(url.contains("flutter-web-renderer=html"));
    }

    /// Integration test: load all real YAML files from config/tests/agora_regression/
    /// and assert structural correctness.  Catches YAML field-name mistakes before
    /// a 30-minute batch run reveals the problem.
    #[test]
    fn agora_regression_yamls_all_parseable_and_valid() {
        let all = load_all();

        // Must include all 12 agora_regression test IDs
        let expected_ids = [
            "agora_admin_category_filter",
            "agora_admin_status_chip",
            "agora_cart_add_remove",
            "agora_checkout_dry",
            "agora_logout_flow",
            "agora_navigation_breadcrumb",
            "agora_notification_delete",
            "agora_pickup_checkboxes_restore",
            "agora_pickup_service_default",
            "agora_pickup_time_picker",
            "agora_search_keyword",
            "agora_webrtc_permission",
        ];
        let loaded_ids: Vec<&str> = all.iter().map(|t| t.id.as_str()).collect();
        for id in &expected_ids {
            assert!(
                loaded_ids.contains(id),
                "test '{}' not found in load_all() — YAML probably failed to parse",
                id
            );
        }

        // Tests that must have a fixture with exactly 5 setup steps
        // (wait + enable_a11y + shadow_click + wait + enable_a11y)
        let fixture_required = [
            "agora_search_keyword",
            "agora_notification_delete",
            "agora_webrtc_permission",
            "agora_logout_flow",
            "agora_navigation_breadcrumb",
            "agora_cart_add_remove",
            "agora_checkout_dry",
            "agora_pickup_time_picker",
            "agora_pickup_checkboxes_restore",
            "agora_admin_status_chip",
            "agora_admin_category_filter",
        ];
        for id in &fixture_required {
            let test = all.iter().find(|t| t.id == *id).unwrap();
            let fixture = test.fixture.as_ref().unwrap_or_else(|| {
                panic!("test '{}' has fixture: None — check YAML fixture.setup nesting", id)
            });
            assert_eq!(
                fixture.setup.len(), 5,
                "test '{}' fixture.setup should have 5 steps (wait+a11y+click+wait+a11y), got {}",
                id, fixture.setup.len()
            );
            assert_eq!(fixture.setup[0].action, "wait",   "step 0 should be wait in '{}'", id);
            assert_eq!(fixture.setup[1].action, "enable_a11y", "step 1 should be enable_a11y in '{}'", id);
            assert_eq!(fixture.setup[2].action, "shadow_click", "step 2 should be shadow_click in '{}'", id);
        }

        // max_iterations sanity: all tests must have >= 20
        for test in &all {
            if expected_ids.contains(&test.id.as_str()) {
                assert!(
                    test.max_iterations >= 20,
                    "test '{}' max_iterations={} is too low (min 20)",
                    test.id, test.max_iterations
                );
            }
        }

        // Correct login persona per test
        let buyer_tests = ["agora_search_keyword", "agora_logout_flow",
            "agora_navigation_breadcrumb", "agora_cart_add_remove",
            "agora_checkout_dry", "agora_pickup_time_picker",
            "agora_notification_delete", "agora_webrtc_permission"];
        let seller_tests = ["agora_pickup_checkboxes_restore"];
        let admin_tests  = ["agora_admin_status_chip", "agora_admin_category_filter"];

        for (ids, expected_name) in [
            (buyer_tests.as_slice(),  "測試買家"),
            (seller_tests.as_slice(), "測試賣家"),
            (admin_tests.as_slice(),  "測試管理員"),
        ] {
            for id in ids {
                let test = all.iter().find(|t| t.id == *id).unwrap();
                if let Some(fix) = &test.fixture {
                    let click = fix.setup.iter().find(|s| s.action == "shadow_click");
                    if let Some(step) = click {
                        // name_regex is an extra field forwarded via FixtureStep.extra
                        let regex = step.extra.get("name_regex")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        assert!(
                            regex.contains(expected_name),
                            "test '{}' should login as '{}' but fixture shadow_click name_regex='{}'",
                            id, expected_name, regex
                        );
                    }
                }
            }
        }
    }
}
