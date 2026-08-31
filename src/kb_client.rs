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
//! - Read-only calls prefer the existing Agora OPS authorization from
//!   `SIRIN_AGORA_OPS_AUTHORIZATION`, `MCP_OPS_KEY`, or the local
//!   `~/.claude.json` `agora-ops` `X-OPS-Authorization` header. This avoids
//!   treating an external-AI bearer or Telegram approval state as read proof.
//! - Explicit KB writes use `SIRIN_KB_SSH_HOST` / `SIRIN_KB_SSH_KEY` (falling
//!   back to `AGORA_SSH_HOST` / `AGORA_SSH_KEY`) and call only `kbWrite` over
//!   the server-local MCP endpoint. The remote credential never leaves the
//!   AgoraMarketAPI host and no per-call Telegram approval is requested.
//!
//! ## Failure handling
//! All errors are logged and returned to the caller — they should treat KB as
//! a best-effort enhancement, never as a hard dependency.  A KB outage must
//! never break a test run.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use crate::platform::NoWindow;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

const KB_WRITE_REMOTE_APP_DIR: &str = "/home/ubuntu/AgoraMarketAPI";
const KB_WRITE_TIMEOUT_SECS: u64 = 75;
const KB_WRITE_PROJECTS: &[&str] = &["sirin", "agora-backend", "flutter"];
const KB_TOPIC_PREFIXES: &[&str] = &[
    "infra-",
    "trading-",
    "ml-",
    "marketplace-",
    "polymarket-",
    "meta-",
    "testing-",
    "sirin-",
    "flutter-",
    "agora-",
    "trap-",
    "store-",
    "dispute-",
];

#[derive(Debug, Clone)]
pub struct KbEntry {
    pub topic_key: String,
    pub title: String,
    pub content: String,
    pub domain: String,
    pub layer: String,
    pub tags: String,
    pub confidence: f64,
    pub file_refs: String,
    pub status: String,
    pub project: String,
}

#[derive(Debug)]
struct KbWriteSshConfig {
    host: String,
    key_path: PathBuf,
}

fn first_non_empty_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn resolve_kb_write_ssh_config() -> Result<KbWriteSshConfig, String> {
    let host = first_non_empty_env(&["SIRIN_KB_SSH_HOST", "AGORA_SSH_HOST"])
        .ok_or("Sirin KB write requires SIRIN_KB_SSH_HOST or AGORA_SSH_HOST")?;
    if host.starts_with('-')
        || host.len() > 255
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | ':' | '-'))
    {
        return Err("Sirin KB SSH host contains unsupported characters".to_string());
    }

    let key_path = PathBuf::from(
        first_non_empty_env(&["SIRIN_KB_SSH_KEY", "AGORA_SSH_KEY"])
            .ok_or("Sirin KB write requires SIRIN_KB_SSH_KEY or AGORA_SSH_KEY")?,
    );
    if !key_path.is_absolute() || !key_path.is_file() {
        return Err("Sirin KB SSH key must be an existing absolute file".to_string());
    }

    Ok(KbWriteSshConfig { host, key_path })
}

fn validate_entry(entry: &KbEntry) -> Result<(), String> {
    if !KB_WRITE_PROJECTS.contains(&entry.project.as_str()) {
        return Err(format!(
            "Sirin KB writes are limited to projects: {}",
            KB_WRITE_PROJECTS.join(", ")
        ));
    }
    if entry.topic_key.len() > 96
        || !KB_TOPIC_PREFIXES
            .iter()
            .any(|prefix| entry.topic_key.starts_with(prefix))
        || !entry
            .topic_key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(
            "topic_key must use a recognized domain prefix and lowercase kebab-case".to_string(),
        );
    }
    if entry.title.trim().is_empty() || entry.title.chars().count() > 200 {
        return Err("title must contain 1-200 characters".to_string());
    }
    if entry.content.trim().is_empty() || entry.content.chars().count() > 3500 {
        return Err("content must contain 1-3500 characters".to_string());
    }
    if entry.domain.trim().is_empty() || entry.domain.len() > 64 {
        return Err("domain must contain 1-64 characters".to_string());
    }
    if !matches!(entry.layer.as_str(), "raw" | "topic" | "brief") {
        return Err("layer must be raw, topic, or brief".to_string());
    }
    if !matches!(entry.status.as_str(), "draft" | "confirmed") {
        return Err("status must be draft or confirmed".to_string());
    }
    if !entry.confidence.is_finite() || !(0.0..=1.0).contains(&entry.confidence) {
        return Err("confidence must be between 0.0 and 1.0".to_string());
    }
    if entry.tags.len() > 1000 || entry.file_refs.len() > 2000 {
        return Err("tags or file_refs exceed the Sirin KB write limit".to_string());
    }
    Ok(())
}

async fn server_local_mcp_request(method: &str, params: Value) -> Result<Value, String> {
    let config = resolve_kb_write_ssh_config()?;
    let payload = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": "sirin-kb-local-service",
        "method": method,
        "params": params,
    }))
    .map_err(|e| format!("serialize Sirin KB MCP request failed: {e}"))?;

    let remote_command = format!(
        r#"set -euo pipefail
cd '{KB_WRITE_REMOTE_APP_DIR}'
PID="$(cat app.pid)"
if ! [[ "$PID" =~ ^[0-9]+$ ]]; then
  echo 'Invalid AgoraMarketAPI application PID' >&2
  exit 2
fi
PORT="$(lsof -Pan -p "$PID" -i 2>/dev/null | awk '/LISTEN/ {{print $9}}' | sed -nE 's/.*:([0-9]+)$/\1/p' | head -1)"
if ! [[ "$PORT" =~ ^[0-9]+$ ]]; then
  echo 'Could not detect AgoraMarketAPI application port' >&2
  exit 3
fi
MCP_KEY="$(tr '\0' '\n' </proc/"$PID"/environ | awk '/^MCP_API_KEY=/ {{sub(/^MCP_API_KEY=/, ""); print; exit}}')"
if [ -z "$MCP_KEY" ]; then
  echo 'Could not read AgoraMarketAPI service credential' >&2
  exit 4
fi
curl -fsS --max-time 65 \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $MCP_KEY" \
  --data-binary @- "http://127.0.0.1:$PORT/api/mcp"
"#
    );

    let mut command = tokio::process::Command::new("ssh");
    command.no_window();
    command
        .arg("-i")
        .arg(&config.key_path)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "LogLevel=ERROR",
        ])
        .arg(&config.host)
        .arg(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|e| format!("start Sirin KB SSH transport failed: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Sirin KB SSH transport did not expose stdin")?;
    stdin
        .write_all(&payload)
        .await
        .map_err(|e| format!("write Sirin KB MCP request failed: {e}"))?;
    drop(stdin);

    let output = tokio::time::timeout(
        Duration::from_secs(KB_WRITE_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| "Sirin KB SSH transport timed out".to_string())?
    .map_err(|e| format!("wait for Sirin KB SSH transport failed: {e}"))?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Sirin KB SSH transport exited {}: {}",
            output.status,
            detail.trim().chars().take(500).collect::<String>()
        ));
    }

    let response: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "AgoraMarketAPI KB MCP returned invalid JSON".to_string())?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown JSON-RPC error");
        return Err(format!(
            "AgoraMarketAPI KB MCP rejected the request: {message}"
        ));
    }
    Ok(response)
}

fn validate_kb_write_policy(response: &Value) -> Result<(), String> {
    let tools = response
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .ok_or("AgoraMarketAPI tools/list did not return a tools array")?;
    let tool = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("kbWrite"))
        .ok_or("AgoraMarketAPI no longer exposes kbWrite")?;
    let auth = tool.get("auth").unwrap_or(&Value::Null);
    let policy_matches = auth.get("requiredLevel").and_then(Value::as_str) == Some("DEV")
        && auth.get("operationMode").and_then(Value::as_str) == Some("WRITE")
        && auth.get("riskLevel").and_then(Value::as_str) == Some("HIGH");
    if !policy_matches {
        return Err("AgoraMarketAPI kbWrite policy drifted from DEV/WRITE/HIGH".to_string());
    }
    Ok(())
}

/// Write one explicitly requested KB entry through Sirin's server-local,
/// tool-scoped service path. The remote MCP credential is read and consumed
/// on the AgoraMarketAPI host and is never returned to Windows or MCP callers.
pub async fn write_entry(entry: &KbEntry) -> Result<Value, String> {
    validate_entry(entry)?;
    let tools = server_local_mcp_request("tools/list", json!({})).await?;
    validate_kb_write_policy(&tools)?;

    let response = server_local_mcp_request(
        "tools/call",
        json!({
            "name": "kbWrite",
            "arguments": {
                "topicKey": entry.topic_key,
                "title": entry.title,
                "content": entry.content,
                "domain": entry.domain,
                "layer": entry.layer,
                "tags": entry.tags,
                "confidence": entry.confidence,
                "fileRefs": entry.file_refs,
                "status": entry.status,
                "source": "sirin",
                "project": entry.project,
            }
        }),
    )
    .await?;
    let result = response.get("result").cloned().unwrap_or(Value::Null);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err("AgoraMarketAPI kbWrite returned an MCP error result".to_string());
    }
    // A successful write may update a topic that was read earlier in this
    // process.  Do not let the one-hour read cache hide that update from the
    // caller's required read-back verification.
    cache::invalidate(&entry.project, &entry.topic_key);
    Ok(result)
}

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

    pub fn invalidate(project: &str, topic_key: &str) {
        let Some(mtx) = db() else { return };
        let conn = mtx.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute(
            "DELETE FROM kb_cache WHERE project = ?1 AND topic_key = ?2",
            rusqlite::params![project, topic_key],
        );
    }
}

/// True when KB integration is enabled via env var.
///
/// Returns `false` when `KB_ENABLED` is unset, empty, or any of `0`/`false`/`no`
/// (case-insensitive).  All KB helpers short-circuit when this is false so
/// callers don't need conditional code.
pub fn enabled() -> bool {
    let v = std::env::var("KB_ENABLED")
        .unwrap_or_default()
        .to_lowercase();
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
    let v = std::env::var("KB_WRITE_TELEMETRY")
        .unwrap_or_default()
        .to_lowercase();
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

/// Read-only Agora OPS authorization, loaded without exposing it through MCP.
pub fn ops_authorization() -> Option<String> {
    for key in ["SIRIN_AGORA_OPS_AUTHORIZATION", "MCP_OPS_KEY"] {
        if let Ok(value) = std::env::var(key) {
            let normalized = normalize_authorization(&value);
            if !normalized.is_empty() {
                return Some(normalized);
            }
        }
    }

    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let source = std::fs::read_to_string(std::path::Path::new(&home).join(".claude.json")).ok()?;
    let value: Value = serde_json::from_str(&source).ok()?;
    ops_authorization_from_claude_json(&value)
}

fn ops_authorization_from_claude_json(value: &Value) -> Option<String> {
    let headers = &value["mcpServers"]["agora-ops"]["headers"];
    [
        "X-OPS-Authorization",
        "X-Ops-Authorization",
        "x-ops-authorization",
    ]
    .iter()
    .find_map(|header| headers[*header].as_str())
    .map(normalize_authorization)
    .filter(|token| !token.is_empty())
}

fn normalize_authorization(value: &str) -> String {
    let trimmed = value.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let scheme = parts.next().unwrap_or_default();
    if scheme.eq_ignore_ascii_case("bearer") {
        parts.next().unwrap_or_default().trim().to_string()
    } else {
        trimmed.to_string()
    }
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

/// Fetch a single KB entry by topicKey. Returns `Ok(None)` when KB is disabled
/// or the upstream returns no textual entry. Transport and authorization
/// failures return `Err` (unless a stale local cache can be served), so the
/// public wrapper never misreports an unavailable KB as a missing topic.
pub async fn get(project: &str, topic_key: &str) -> Result<Option<String>, String> {
    if !enabled() {
        return Ok(None);
    }
    // Fresh-cache fast path — skip network entirely.
    if let Some((content, age)) = cache::get(project, topic_key) {
        if cache::is_fresh(age) {
            if let Some(content) = normalize_upstream_text(&content) {
                tracing::debug!("[kb_client] cache hit (fresh, {age}s): {project}/{topic_key}");
                return Ok(Some(content));
            }
            // Older Sirin builds could cache textual "Not found" responses.
            // Evict them so a later write becomes visible immediately.
            cache::invalidate(project, topic_key);
        }
    }

    let args = json!({ "topicKey": topic_key, "project": project });
    let ops_authorization = ops_authorization();
    let fallback_bearer = ops_authorization.is_none().then(bearer).flatten();
    let res = match crate::mcp_client::call_tool_with_auth_headers(
        &url(),
        "kbGet",
        args,
        fallback_bearer.as_deref(),
        ops_authorization.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            // Network failed — fall back to (possibly stale) cache.
            if let Some((content, age)) = cache::get(project, topic_key) {
                if let Some(content) = normalize_upstream_text(&content) {
                    tracing::warn!(
                        "[kb_client] kbGet({project}/{topic_key}) failed: {e}; \
                         serving stale cache ({age}s old)"
                    );
                    return Ok(Some(content));
                }
                cache::invalidate(project, topic_key);
            }
            tracing::warn!("[kb_client] kbGet({project}/{topic_key}) failed: {e}; no cache");
            return Err(format!("kbGet({project}/{topic_key}) failed: {e}"));
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
pub async fn search(project: &str, query: &str, limit: usize) -> Result<Option<String>, String> {
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
    let ops_authorization = ops_authorization();
    let fallback_bearer = ops_authorization.is_none().then(bearer).flatten();
    let res = match crate::mcp_client::call_tool_with_auth_headers(
        &url(),
        "kbSearch",
        args,
        fallback_bearer.as_deref(),
        ops_authorization.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("[kb_client] kbSearch({project}, {query:?}) failed: {e}");
            return Ok(None);
        }
    };
    Ok(extract_text(&res))
}

/// Fetch the upstream KB health report through the same authorization and
/// transport path as reads. Unlike search, health is diagnostic evidence: an
/// unavailable/empty response is returned as an error, never as healthy.
pub async fn health(project: &str) -> Result<String, String> {
    if !enabled() {
        return Err("KB disabled (set KB_ENABLED=1)".to_string());
    }
    let args = json!({ "project": project });
    let ops_authorization = ops_authorization();
    let fallback_bearer = ops_authorization.is_none().then(bearer).flatten();
    let response = crate::mcp_client::call_tool_with_auth_headers(
        &url(),
        "kbHealth",
        args,
        fallback_bearer.as_deref(),
        ops_authorization.as_deref(),
    )
    .await
    .map_err(|e| format!("kbHealth({project}) failed: {e}"))?;
    extract_text(&response).ok_or_else(|| format!("kbHealth({project}) returned no usable content"))
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
    write_raw_to_project(
        &default_project(),
        topic_key,
        title,
        content,
        domain,
        tags,
        file_refs,
    )
    .await
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
    let entry = KbEntry {
        topic_key: topic_key.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        domain: domain.to_string(),
        layer: "raw".to_string(),
        tags: tags.to_string(),
        confidence: 0.6,
        file_refs: file_refs.to_string(),
        status: "draft".to_string(),
        project: project.to_string(),
    };
    write_entry(&entry).await.map(|_| ())
}

/// Helper: pull the textual body out of an MCP tool response.
///
/// `mcp_client::call_tool` already collapses MCP `content[].text` into
/// `{"result": "<text>"}`.  Strip the layer here so callers get a plain
/// `Option<String>`.
fn extract_text(v: &Value) -> Option<String> {
    if let Some(s) = v.get("result").and_then(Value::as_str) {
        if let Some(text) = normalize_upstream_text(s) {
            return Some(text);
        }
    }
    if let Some(s) = v.as_str() {
        if let Some(text) = normalize_upstream_text(s) {
            return Some(text);
        }
    }
    None
}

fn normalize_upstream_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Some upstream MCP wrappers serialize the text body once more, producing
    // a JSON string such as `"Not found: topic"`. Decode that layer before
    // classifying the response.
    let decoded = serde_json::from_str::<String>(trimmed).unwrap_or_else(|_| trimmed.to_string());
    let text = decoded.trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    let is_negative = lower.starts_with("not found:")
        || lower.starts_with("no results")
        || lower.starts_with("no result found")
        || lower.starts_with("no kb entry found");
    (!is_negative).then(|| text.to_string())
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
    fn ops_authorization_uses_only_x_ops_header() {
        let value = json!({
            "mcpServers": {
                "agora-ops": {
                    "headers": {
                        "Authorization": "Bearer external-ai-token",
                        "X-OPS-Authorization": "ops-read-token"
                    }
                }
            }
        });
        assert_eq!(
            ops_authorization_from_claude_json(&value).as_deref(),
            Some("ops-read-token")
        );
    }

    #[test]
    fn ops_authorization_never_falls_back_to_external_ai_bearer() {
        let value = json!({
            "mcpServers": {
                "agora-ops": {
                    "headers": { "Authorization": "Bearer external-ai-token" }
                }
            }
        });
        assert_eq!(ops_authorization_from_claude_json(&value), None);
        assert_eq!(normalize_authorization(" Bearer ops-token "), "ops-token");
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

    #[test]
    fn negative_upstream_text_is_not_treated_as_kb_content() {
        assert_eq!(
            extract_text(&json!({"result": "Not found: sirin-missing"})),
            None
        );
        assert_eq!(
            extract_text(&json!({"result": "\"Not found: sirin-missing\""})),
            None
        );
        assert_eq!(
            extract_text(&json!({"result": "\"No results for query\""})),
            None
        );
        assert_eq!(
            extract_text(&json!({"result": "confirmed KB content"})).as_deref(),
            Some("confirmed KB content")
        );
    }

    fn valid_entry() -> KbEntry {
        KbEntry {
            topic_key: "sirin-kb-local-write".to_string(),
            title: "Sirin KB local write".to_string(),
            content: "Sirin uses a fixed server-local kbWrite service path.".to_string(),
            domain: "tooling".to_string(),
            layer: "topic".to_string(),
            tags: "sirin,kb".to_string(),
            confidence: 0.9,
            file_refs: "src/kb_client.rs".to_string(),
            status: "confirmed".to_string(),
            project: "sirin".to_string(),
        }
    }

    #[test]
    fn manual_write_validation_is_project_and_topic_scoped() {
        let entry = valid_entry();
        assert!(validate_entry(&entry).is_ok());

        let mut wrong_project = entry.clone();
        wrong_project.project = "payments".to_string();
        assert!(validate_entry(&wrong_project).is_err());

        let mut wrong_topic = entry;
        wrong_topic.topic_key = "NoPrefix_or_Underscore".to_string();
        assert!(validate_entry(&wrong_topic).is_err());
    }

    #[test]
    fn kb_write_policy_must_remain_dev_write_high() {
        let expected = json!({
            "result": {
                "tools": [{
                    "name": "kbWrite",
                    "auth": {
                        "requiredLevel": "DEV",
                        "operationMode": "WRITE",
                        "riskLevel": "HIGH"
                    }
                }]
            }
        });
        assert!(validate_kb_write_policy(&expected).is_ok());

        let drifted = json!({
            "result": {
                "tools": [{
                    "name": "kbWrite",
                    "auth": {
                        "requiredLevel": "OPS",
                        "operationMode": "WRITE",
                        "riskLevel": "HIGH"
                    }
                }]
            }
        });
        assert!(validate_kb_write_policy(&drifted).is_err());
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
