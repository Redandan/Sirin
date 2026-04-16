//! YAML parsing for test goals.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    #[serde(default)]
    pub success_criteria: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_max_iter() -> u32 { 15 }
fn default_timeout() -> u64 { 120 }

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
    fn missing_required_fields_fails() {
        let yaml = "id: x\n";
        let r: Result<TestGoal, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err());
    }
}
