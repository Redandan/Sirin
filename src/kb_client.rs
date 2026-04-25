//! Thin client for the agora-trading Knowledge Base MCP server.
//!
//! Wraps [`crate::mcp_client::call_tool`] with KB-specific helpers:
//! - [`get`] — fetch a single entry by topicKey
//! - [`search`] — semantic search returning top-N hits
//! - [`write_raw`] — write a `layer="raw"` note from runtime (used by
//!   convergence guard, triage, etc. to capture failure patterns)
//!
//! ## Configuration
//! - `KB_ENABLED` — `1`/`true` to enable KB calls (default: `0`).  When
//!   disabled all helpers short-circuit with `Ok(None)` / `Ok(())` so callers
//!   can opt-in without changing call sites.
//! - `KB_MCP_URL` — MCP server endpoint (default `http://localhost:3001/mcp`).
//! - `KB_PROJECT` — default project slug for [`write_raw`] (default `sirin`).
//!
//! ## Failure handling
//! All errors are logged and returned to the caller — they should treat KB as
//! a best-effort enhancement, never as a hard dependency.  A KB outage must
//! never break a test run.

use serde_json::{json, Value};

/// True when KB integration is enabled via env var.
///
/// Returns `false` when `KB_ENABLED` is unset, empty, or any of `0`/`false`/`no`
/// (case-insensitive).  All KB helpers short-circuit when this is false so
/// callers don't need conditional code.
pub fn enabled() -> bool {
    let v = std::env::var("KB_ENABLED").unwrap_or_default().to_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}

/// MCP endpoint URL, env-overridable.
pub fn url() -> String {
    std::env::var("KB_MCP_URL").unwrap_or_else(|_| "http://localhost:3001/mcp".into())
}

/// Default project slug for write_raw, env-overridable.
pub fn default_project() -> String {
    std::env::var("KB_PROJECT").unwrap_or_else(|_| "sirin".into())
}

/// Detect whether a string looks like a KB `topicKey` (kebab-case with project
/// prefix) vs a filesystem path.  Used by docs_refs auto-resolution.
///
/// Heuristic:
/// - Contains a path separator (`/` or `\`) → path
/// - Contains a `.` followed by 2-5 letters (file extension) → path
/// - Otherwise → topicKey
pub fn looks_like_topic_key(s: &str) -> bool {
    if s.contains('/') || s.contains('\\') {
        return false;
    }
    if let Some(idx) = s.rfind('.') {
        let ext = &s[idx + 1..];
        if !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphabetic()) {
            return false;
        }
    }
    true
}

/// Fetch a single KB entry by topicKey.  Returns `Ok(None)` when KB is
/// disabled, when the entry doesn't exist, or on transport error (the
/// underlying error is logged; callers see the same Ok(None) shape so they
/// don't need to special-case "down" vs "missing").
pub async fn get(project: &str, topic_key: &str) -> Result<Option<String>, String> {
    if !enabled() {
        return Ok(None);
    }
    let args = json!({ "topicKey": topic_key, "project": project });
    let res = match crate::mcp_client::call_tool(&url(), "kbGet", args).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("[kb_client] kbGet({project}/{topic_key}) failed: {e}");
            return Ok(None);
        }
    };
    Ok(extract_text(&res))
}

/// Semantic search over the KB.  Returns at most `limit` ranked hit bodies
/// concatenated as a single string ready to splice into a prompt.
///
/// Falls through to `Ok(None)` on any failure — callers treat search as a
/// best-effort context enhancement.
#[allow(dead_code)]
pub async fn search(
    project: &str,
    query: &str,
    limit: usize,
) -> Result<Option<String>, String> {
    if !enabled() {
        return Ok(None);
    }
    let args = json!({
        "project": project,
        "query":   query,
        "domain":  "",
        "layer":   "",
        "status":  "",
        "limit":   limit as i64,
    });
    let res = match crate::mcp_client::call_tool(&url(), "kbSearch", args).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("[kb_client] kbSearch({project}, {query:?}) failed: {e}");
            return Ok(None);
        }
    };
    Ok(extract_text(&res))
}

/// Append-only writer for runtime-discovered `raw`-layer notes (failure
/// patterns, stuck-loops, etc).  Uses the default project from `KB_PROJECT`
/// unless caller specifies otherwise via [`write_raw_to_project`].
///
/// `topic_key` should be deterministic for the same observation so repeated
/// occurrences upsert (kbWrite auto-versions) rather than spamming new keys.
pub async fn write_raw(
    topic_key: &str,
    title: &str,
    content: &str,
    domain: &str,
    tags: &str,
    file_refs: &str,
) -> Result<(), String> {
    write_raw_to_project(&default_project(), topic_key, title, content, domain, tags, file_refs).await
}

/// Lower-level form: write a raw note to an explicit project slug.
pub async fn write_raw_to_project(
    project: &str,
    topic_key: &str,
    title: &str,
    content: &str,
    domain: &str,
    tags: &str,
    file_refs: &str,
) -> Result<(), String> {
    if !enabled() {
        return Ok(());
    }
    let args = json!({
        "topicKey":   topic_key,
        "title":      title,
        "content":    content,
        "domain":     domain,
        "layer":      "raw",
        "tags":       tags,
        "confidence": 0.6_f64,
        "fileRefs":   file_refs,
        "status":     "draft",
        "source":     "sirin",
        "project":    project,
    });
    if let Err(e) = crate::mcp_client::call_tool(&url(), "kbWrite", args).await {
        tracing::warn!("[kb_client] kbWrite({project}/{topic_key}) failed: {e}");
        return Err(e);
    }
    tracing::debug!("[kb_client] wrote raw note {project}/{topic_key}");
    Ok(())
}

/// Helper: pull the textual body out of an MCP tool response.
///
/// `mcp_client::call_tool` already collapses MCP `content[].text` into
/// `{"result": "<text>"}`.  Strip the layer here so callers get a plain
/// `Option<String>`.
fn extract_text(v: &Value) -> Option<String> {
    if let Some(s) = v.get("result").and_then(Value::as_str) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(s) = v.as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    None
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_off_by_default() {
        std::env::remove_var("KB_ENABLED");
        assert!(!enabled());
    }

    #[test]
    fn enabled_recognises_truthy_values() {
        for v in ["1", "true", "True", "TRUE", "yes", "on"] {
            std::env::set_var("KB_ENABLED", v);
            assert!(enabled(), "expected enabled for {v}");
        }
        std::env::remove_var("KB_ENABLED");
    }

    #[test]
    fn enabled_recognises_falsy_values() {
        for v in ["0", "false", "no", "off", ""] {
            std::env::set_var("KB_ENABLED", v);
            assert!(!enabled(), "expected disabled for {v:?}");
        }
        std::env::remove_var("KB_ENABLED");
    }

    #[test]
    fn url_default_when_unset() {
        std::env::remove_var("KB_MCP_URL");
        assert_eq!(url(), "http://localhost:3001/mcp");
    }

    #[test]
    fn url_honours_env_override() {
        std::env::set_var("KB_MCP_URL", "http://kb.internal:9000/mcp");
        assert_eq!(url(), "http://kb.internal:9000/mcp");
        std::env::remove_var("KB_MCP_URL");
    }

    #[test]
    fn topic_key_detection_recognises_kebab_keys() {
        assert!(looks_like_topic_key("sirin-test-authoring"));
        assert!(looks_like_topic_key("agora-pickup-flow"));
        assert!(looks_like_topic_key("flutter-canvaskit-traps"));
        assert!(looks_like_topic_key("simple"));
    }

    #[test]
    fn topic_key_detection_rejects_paths() {
        assert!(!looks_like_topic_key("docs/agora-market.md"));
        assert!(!looks_like_topic_key(".claude/skills/sirin-test/SKILL.md"));
        assert!(!looks_like_topic_key("/abs/path/to/file"));
        assert!(!looks_like_topic_key("C:\\Users\\foo\\file.txt"));
    }

    #[test]
    fn topic_key_detection_rejects_extensions() {
        assert!(!looks_like_topic_key("README.md"));
        assert!(!looks_like_topic_key("config.yaml"));
        assert!(!looks_like_topic_key("test.rs"));
        // Numeric "extension" doesn't count — issue numbers etc.
        assert!(looks_like_topic_key("issue-1234"));
        // Long suffix doesn't count as ext
        assert!(looks_like_topic_key("v0.4.4-release"));
    }

    /// All KB helpers must be no-ops (Ok(None) / Ok(())) when KB_ENABLED is
    /// false — callers shouldn't need conditional code.  Live network calls
    /// are skipped here; we only assert the disabled short-circuit path.
    #[tokio::test]
    async fn helpers_short_circuit_when_disabled() {
        std::env::remove_var("KB_ENABLED");
        assert!(get("sirin", "anything").await.unwrap().is_none());
        assert!(search("sirin", "any query", 3).await.unwrap().is_none());
        assert!(write_raw("k", "t", "c", "d", "tag", "f.rs").await.is_ok());
    }
}
