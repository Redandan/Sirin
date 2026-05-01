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

mod cache {
    //! Local SQLite cache for KB entries — offline fallback when KB server
    //! is down (Issue #37).  Single process-wide connection at
    //! `<app_data_dir>/kb_cache.db`.
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Cache freshness window in seconds.  Hits younger than this skip the
    /// network entirely; older hits still serve as fallback when the network
    /// fails.
    const FRESH_TTL_SECS: u64 = 3600;

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn db() -> Option<&'static Mutex<rusqlite::Connection>> {
        static DB: OnceLock<Option<Mutex<rusqlite::Connection>>> = OnceLock::new();
        DB.get_or_init(|| {
            let path = crate::platform::app_data_dir().join("kb_cache.db");
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let conn = rusqlite::Connection::open(&path).ok()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS kb_cache ( \
                     project    TEXT NOT NULL, \
                     topic_key  TEXT NOT NULL, \
                     content    TEXT NOT NULL, \
                     fetched_at INTEGER NOT NULL, \
                     PRIMARY KEY (project, topic_key) \
                 );",
            )
            .ok()?;
            Some(Mutex::new(conn))
        })
        .as_ref()
    }

    /// Returns `(content, age_secs)` if a cached entry exists.
    pub fn get(project: &str, topic_key: &str) -> Option<(String, i64)> {
        let mtx = db()?;
        let conn = mtx.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT content, fetched_at FROM kb_cache \
             WHERE project = ?1 AND topic_key = ?2",
            rusqlite::params![project, topic_key],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .ok()
        .map(|(content, fetched_at)| (content, now_secs() - fetched_at))
    }

    /// Cache hit is fresh (no need to hit network).
    pub fn is_fresh(age_secs: i64) -> bool {
        age_secs >= 0 && (age_secs as u64) < FRESH_TTL_SECS
    }

    pub fn set(project: &str, topic_key: &str, content: &str) {
        let Some(mtx) = db() else { return };
        let conn = mtx.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute(
            "INSERT INTO kb_cache (project, topic_key, content, fetched_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(project, topic_key) DO UPDATE SET \
                 content = excluded.content, \
                 fetched_at = excluded.fetched_at",
            rusqlite::params![project, topic_key, content, now_secs()],
        );
    }
}

/// True when KB integration is enabled via env var.
///
/// Returns `false` when `KB_ENABLED` is unset, empty, or any of `0`/`false`/`no`
/// (case-insensitive).  All KB helpers short-circuit when this is false so
/// callers don't need conditional code.
pub fn enabled() -> bool {
    let v = std::env::var("KB_ENABLED").unwrap_or_default().to_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}

/// True when per-run telemetry writes (pass/fail/triage/stuck-loop notes) are
/// enabled.  Defaults to **false** to avoid KB draft pollution.
///
/// These raw-layer notes (sirin-pass-*, sirin-failure-*, yaml-dispute-*, etc.)
/// have short-term debug value but accumulate quickly and dilute semantic search.
/// Set `KB_WRITE_TELEMETRY=1` to re-enable (e.g. during a debugging session).
///
/// Long-term knowledge should be written by Claude sessions via `kbWrite` directly
/// with `layer=topic, status=confirmed` — not by automated test runs.
///
/// Issue: #218
pub fn telemetry_write_enabled() -> bool {
    let v = std::env::var("KB_WRITE_TELEMETRY").unwrap_or_default().to_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}

/// MCP endpoint URL, env-overridable.
///
/// Default `http://localhost:3001/mcp` covers the local-dev case where a
/// user runs the KB MCP server on their machine.  For the hosted agora-
/// trading service, set `KB_MCP_URL=https://agoramarketapi.purrtechllc.com/api/mcp`
/// + `KB_MCP_BEARER=<token>` (see `~/.claude.json` for the existing token
/// configured for Claude Code).
pub fn url() -> String {
    std::env::var("KB_MCP_URL").unwrap_or_else(|_| "http://localhost:3001/mcp".into())
}

/// Optional Bearer token for the KB MCP endpoint.  `None` when unset.
///
/// Hosted KB endpoints (e.g. agora-trading) require auth — supply the same
/// token Claude Code's MCP config carries (`~/.claude.json` →
/// `mcpServers.agora-trading.headers.Authorization`).
pub fn bearer() -> Option<String> {
    std::env::var("KB_MCP_BEARER")
        .ok()
        .filter(|v| !v.trim().is_empty())
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
    // Fresh-cache fast path — skip network entirely.
    if let Some((content, age)) = cache::get(project, topic_key) {
        if cache::is_fresh(age) {
            tracing::debug!(
                "[kb_client] cache hit (fresh, {age}s): {project}/{topic_key}"
            );
            return Ok(Some(content));
        }
    }

    let args = json!({ "topicKey": topic_key, "project": project });
    let res = match crate::mcp_client::call_tool_with_bearer(
        &url(), "kbGet", args, bearer().as_deref()
    ).await {
        Ok(r) => r,
        Err(e) => {
            // Network failed — fall back to (possibly stale) cache.
            if let Some((content, age)) = cache::get(project, topic_key) {
                tracing::warn!(
                    "[kb_client] kbGet({project}/{topic_key}) failed: {e}; \
                     serving stale cache ({age}s old)"
                );
                return Ok(Some(content));
            }
            tracing::debug!(
                "[kb_client] kbGet({project}/{topic_key}) failed: {e}; no cache"
            );
            return Ok(None);
        }
    };
    let text = extract_text(&res);
    if let Some(ref content) = text {
        cache::set(project, topic_key, content);
    }
    Ok(text)
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
    let res = match crate::mcp_client::call_tool_with_bearer(
        &url(), "kbSearch", args, bearer().as_deref()
    ).await {
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
    // Guard 1: KB must be enabled at all.
    // Guard 2: Per-run telemetry writes are OFF by default to prevent KB draft
    //          pollution (sirin-pass-*, sirin-failure-*, yaml-dispute-*, etc.).
    //          Set KB_WRITE_TELEMETRY=1 to re-enable during debugging. (#218)
    if !enabled() || !telemetry_write_enabled() {
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
    // Retry up to 3× with exponential backoff (2s → 4s).
    // KB write is best-effort — we log and return Err on final failure so
    // callers (triage.rs) can ignore if they prefer, but we don't silently drop.
    let mut last_err = String::new();
    for attempt in 0u32..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2u64 << (attempt - 1))).await;
        }
        match crate::mcp_client::call_tool_with_bearer(
            &url(), "kbWrite", args.clone(), bearer().as_deref()
        ).await {
            Ok(_) => {
                tracing::debug!("[kb_client] wrote raw note {project}/{topic_key}");
                return Ok(());
            }
            Err(e) => {
                last_err = e.to_string();
                tracing::warn!(
                    "[kb_client] kbWrite({project}/{topic_key}) failed (attempt {}/3): {e}",
                    attempt + 1
                );
            }
        }
    }
    Err(last_err)
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
