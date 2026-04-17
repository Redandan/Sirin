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
    #[serde(default)]
    pub success_criteria: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional fixture: setup steps (before test) and cleanup steps (after).
    #[serde(default)]
    pub fixture: Option<Fixture>,
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

fn default_max_iter() -> u32 { 15 }
fn default_timeout() -> u64 { 120 }
fn default_parse_retries() -> u32 { 3 }
fn default_locale() -> String { "zh-TW".into() }

/// Directory containing YAML test definitions.
fn tests_dir() -> PathBuf {
    PathBuf::from("config").join("tests")
}

/// Load all YAML tests from `config/tests/`.
pub fn load_all() -> Vec<TestGoal> {
    let dir = tests_dir();
    if !dir.exists() { return Vec::new(); }
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("yaml")
               && p.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            match load_file(&p) {
                Ok(g) => out.push(g),
                Err(e) => tracing::warn!("Failed to load test {p:?}: {e}"),
            }
        }
    }
    out
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
        assert_eq!(g.max_iterations, 15); // default
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
}
