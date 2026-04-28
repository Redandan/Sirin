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
    /// Paths are relative to the repo root (or absolute).  `docs_refs` accepts
    /// MIXED entries — bare kebab-case identifiers (e.g. `sirin-test-authoring`)
    /// are auto-treated as KB topicKeys.  For *unambiguous* KB-only references
    /// use [`Self::kb_refs`] instead.
    #[serde(default)]
    pub docs_refs: Vec<String>,
    /// Knowledge base topicKeys (no path heuristic) auto-fetched from the
    /// agora-trading KB at run start and spliced into the LLM prompt under
    /// "Required reading".  Use this when you want explicit KB-only
    /// references — `docs_refs` works for the mixed case but a separate field
    /// makes intent clearer and avoids the path-vs-key heuristic edge cases.
    ///
    /// Format: kebab-case slugs matching KB `topicKey`, e.g.
    /// `["sirin-test-authoring", "sirin-browser-automation"]`.
    ///
    /// Resolved via [`crate::kb_client::get`] using project from `KB_PROJECT`
    /// env (default `sirin`).  Failures degrade to "[unavailable: …]" in the
    /// prompt — never aborts a run.  Resolution short-circuits when
    /// `KB_ENABLED` is unset.
    #[serde(default)]
    pub kb_refs: Vec<String>,
    /// How the ReAct loop should observe the page before each LLM turn.
    /// - `text`   — legacy: no screenshot, truncated text observations only
    /// - `vision` — always screenshot + vision LLM call
    /// - `auto`   — screenshot only when Flutter / canvas is detected at runtime
    ///
    /// Default `text` → zero behavioural change for existing tests.  Opt-in
    /// explicitly on Flutter / canvas pages where the AX tree is unreliable.
    #[serde(default)]
    pub perception: crate::perception::PerceptionMode,
    /// Whether to inject a CSS privacy mask before every screenshot taken
    /// during this test (password / credit-card / OTP / SSN inputs are
    /// blurred + colour-stripped so plaintext cannot leak into
    /// `test_failures/`, vision LLM uploads, or GitHub bug reports).
    ///
    /// Default `true` (fail-secure — Issue #80).  Set to `false` only when
    /// you are deliberately verifying that an input renders a secret value
    /// (e.g. testing the masking itself), or when the mask interferes with
    /// the assertion (rare).  The process-wide default is also overridable
    /// via `SIRIN_PRIVACY_MASK=0` env var.
    #[serde(default)]
    pub mask_sensitive: Option<bool>,
    /// Per-test glob patterns appended to the process-wide URL blocklist
    /// (see [`crate::authz::check_blocked_url`] / Issue #81).
    ///
    /// Use this to declare "this test must NEVER navigate to X" — e.g.
    /// `*/payment/confirm` to prevent a dry-run test from completing a
    /// real purchase.  Matching is glob-based (same syntax as
    /// `deny[].url_pattern`).
    #[serde(default)]
    pub blocked_url_patterns: Vec<String>,
    /// Capture an action-annotated GIF timeline of the run (Issue #78).
    /// When `true` (default), every ReAct iteration captures a frame and on
    /// failure the frames are encoded into `test_failures/<run_id>/timeline.gif`
    /// (the single failure-screenshot path stays for back-compat).
    /// Set to `false` to disable for tests where the extra screenshot per step
    /// is too expensive (slow Flutter pages).
    #[serde(default = "default_record_timeline_gif")]
    pub record_timeline_gif: bool,
    /// Inject an in-page action indicator (right-bottom badge + faint border)
    /// during this run so the user can SEE that Sirin is driving the page
    /// (Issue #75 —对标 CiC's agent-visual-indicator).  The badge text
    /// updates to the current ReAct action label on every iteration; the
    /// indicator is hidden automatically before each screenshot / AX-tree
    /// observation so it never pollutes failure captures or the LLM's view
    /// of the page.
    ///
    /// Default `false` — headless CI and parallel batch runs need a clean
    /// DOM.  Opt in only for interactive demos / supervised runs where the
    /// extra DOM nodes are wanted.  Note: the indicator is **UX, not a
    /// security boundary** — page JS can read its existence trivially.
    #[serde(default)]
    pub show_action_indicator: bool,
    /// Number of automatic retries when the test ends with `timeout` or
    /// `error` (transient failures due to Chrome instability, network blips,
    /// etc.).  Does NOT retry on `failed` (logic failure) or `disputed`.
    ///
    /// Default 0 (no retry).  Set to 1-2 for Flutter H5 tests where Chrome
    /// initialization can occasionally stall on the first run.
    ///
    /// Each retry gets a fresh run_id; the last attempt's result is what
    /// `spawn_run_async` reports to callers.
    #[serde(default)]
    pub max_retries: u32,
    /// Override the browser viewport for this test.
    ///
    /// Use this to match the intended device profile:
    /// - Buyer H5 (mobile web): `{width: 390, height: 844, scale: 2.0, mobile: true}`
    /// - Seller PC dashboard:   `{width: 1280, height: 900, scale: 1.0, mobile: false}`
    ///
    /// When omitted, Sirin uses the process-wide default viewport from
    /// `SIRIN_DEFAULT_VIEWPORT` (typically 1440×1600).
    /// ⚠️  Saved scripts embed the viewport used at recording time — running
    /// a script at a different viewport may fail (layout and element positions
    /// differ between mobile and desktop).
    #[serde(default)]
    pub viewport: Option<TestViewport>,
}

/// Viewport configuration for a test.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TestViewport {
    pub width: u32,
    pub height: u32,
    /// Device pixel ratio (CSS pixels → physical pixels).  Default 1.0.
    #[serde(default = "default_scale")]
    pub scale: f64,
    /// Whether to emulate a mobile device (touch events, mobile UA, etc).
    #[serde(default)]
    pub mobile: bool,
}

fn default_scale() -> f64 { 1.0 }

fn default_record_timeline_gif() -> bool { true }

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
fn default_parse_retries() -> u32 { 5 }
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
        assert!(g.kb_refs.is_empty());
    }

    #[test]
    fn parse_yaml_with_kb_refs() {
        let yaml = r#"
id: agora_pickup
name: "Pickup test"
url: "https://example.com"
goal: "verify pickup flow"
kb_refs:
  - sirin-test-authoring
  - sirin-browser-automation
  - agora-pickup-flow
"#;
        let g: TestGoal = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(g.kb_refs.len(), 3);
        assert_eq!(g.kb_refs[0], "sirin-test-authoring");
        assert!(g.docs_refs.is_empty());
    }

    #[test]
    fn parse_yaml_with_both_docs_refs_and_kb_refs() {
        let yaml = r#"
id: combined
name: "Combined refs"
url: "https://example.com"
goal: "test mixed references"
docs_refs:
  - docs/acceptance/issue_42.md
kb_refs:
  - sirin-test-authoring
"#;
        let g: TestGoal = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(g.docs_refs.len(), 1);
        assert_eq!(g.kb_refs.len(), 1);
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

        // Since 2026-04-24 these tests use URL auto-login (?__test_role=) instead
        // of the old shadow_click login fixture.  Executor calls
        // Storage.clearDataForOrigin + 8 s wait + enable_a11y before the ReAct loop.
        // None of the 12 agora_regression tests should require a fixture anymore.
        for id in &expected_ids {
            let test = all.iter().find(|t| t.id == *id).unwrap();
            assert!(
                test.fixture.is_none(),
                "test '{}' has a fixture — should use ?__test_role= URL auto-login instead",
                id
            );
        }

        // All 12 tests must target redandan.github.io with a __test_role= param
        for id in &expected_ids {
            let test = all.iter().find(|t| t.id == *id).unwrap();
            assert!(
                test.url.contains("redandan.github.io"),
                "test '{}' url '{}' should target redandan.github.io",
                id, test.url
            );
            assert!(
                test.url.contains("__test_role="),
                "test '{}' url '{}' must contain __test_role= for auto-login",
                id, test.url
            );
        }

        // headless mode — must NOT explicitly set browser_headless: true.
        // Since v0.4.3 the process-wide default (SIRIN_BROWSER_HEADLESS=false in .env)
        // covers all Flutter CanvasKit tests; per-YAML field is now optional (None = use default).
        for id in &expected_ids {
            let test = all.iter().find(|t| t.id == *id).unwrap();
            assert_ne!(
                test.browser_headless,
                Some(true),
                "test '{}' must not set browser_headless: true (Flutter CanvasKit needs WebGL, \
                 use .env SIRIN_BROWSER_HEADLESS=false for process-wide default)",
                id
            );
        }

        // max_iterations sanity: must be at least 1, no floor of 20 required
        // (individual tests are tuned for their own complexity)
        for id in &expected_ids {
            let test = all.iter().find(|t| t.id == *id).unwrap();
            assert!(
                test.max_iterations >= 1,
                "test '{}' max_iterations={} must be at least 1",
                id, test.max_iterations
            );
        }

        // Correct login role per test (via __test_role= URL param)
        let buyer_tests  = ["agora_search_keyword", "agora_logout_flow",
            "agora_navigation_breadcrumb", "agora_cart_add_remove",
            "agora_checkout_dry",
            "agora_notification_delete", "agora_webrtc_permission"];
        // agora_pickup_time_picker edits seller product-form pickup settings → seller role
        let seller_tests = ["agora_pickup_checkboxes_restore", "agora_pickup_service_default",
            "agora_pickup_time_picker"];
        let admin_tests  = ["agora_admin_status_chip", "agora_admin_category_filter"];

        for (ids, role) in [
            (buyer_tests.as_slice(),  "buyer"),
            (seller_tests.as_slice(), "seller"),
            (admin_tests.as_slice(),  "admin"),
        ] {
            for id in ids {
                let test = all.iter().find(|t| t.id == *id).unwrap();
                assert!(
                    test.url.contains(&format!("__test_role={role}")),
                    "test '{}' url '{}' should contain __test_role={role}",
                    id, test.url
                );
            }
        }
    }
}
