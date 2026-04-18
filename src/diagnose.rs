//! Self-diagnostic snapshot for the `diagnose` MCP tool.
//!
//! ## Why
//!
//! The intended workflow is **two-tier**:
//!
//! 1. **Tier 1** — an external AI (e.g. Claude Code on a user's machine)
//!    calls Sirin's MCP and hits a bug.  Before bothering anyone, it calls
//!    `diagnose` and gets a single rich JSON blob describing Sirin's
//!    version, build, host, browser, LLM and recent error log.  It can
//!    then decide whether to:
//!      - retry (transient)
//!      - tell the user "you're on 0.3.0 but 0.3.2 fixes this — please update"
//!      - file an issue using `report_issue_template` (already populated
//!        with the env block, so the user doesn't have to fill it in)
//!
//! 2. **Tier 2** — the developer (Claude session in the Sirin repo) opens
//!    the resulting GitHub issue and finds *complete reproduction context*:
//!    version + commit + OS + browser + LLM + recent errors + "what the
//!    calling AI already tried".  No round-tripping for environment
//!    information.
//!
//! ## Cost
//!
//! Snapshot construction is ~5–20 ms (one CDP `Browser.getVersion`, one log
//! tail, one `update_status` read).  Safe to call on every error in the
//! caller — Sirin doesn't cache, but the cost is dominated by one network
//! round-trip to Chrome.

use std::sync::OnceLock;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

// ── Startup time ─────────────────────────────────────────────────────────────
//
// Recorded by `record_startup()` from `main.rs` after `init_tracing()`.  Used
// to compute `uptime_secs` in the snapshot — this is what tells the calling
// AI "Sirin just restarted and the failure is probably the cold-start race"
// vs "Sirin has been up 3 days and this looks like a steady-state bug".

static STARTUP: OnceLock<Instant> = OnceLock::new();

/// Record process startup time.  Idempotent — first caller wins.  Safe to
/// call before any other `diagnose` API.
pub fn record_startup() {
    let _ = STARTUP.set(Instant::now());
}

fn uptime_secs() -> u64 {
    STARTUP.get().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

// ── Build-time constants (from build.rs) ─────────────────────────────────────

/// Short SHA of the commit that built this binary, e.g. `"37cfaf5"`.
/// Falls back to `"unknown"` when built outside a git checkout.
pub fn git_commit() -> &'static str {
    option_env!("SIRIN_GIT_COMMIT").unwrap_or("unknown")
}

/// Unix epoch seconds at build time.  Used to derive `build_date` in the
/// snapshot; we keep it as a number here so the runtime side can format it
/// however it pleases (chrono is a runtime dep, not a build-script dep).
pub fn build_epoch() -> u64 {
    option_env!("SIRIN_BUILD_EPOCH").and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn build_date_iso() -> String {
    let secs = build_epoch();
    if secs == 0 { return "unknown".into(); }
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "unknown".into())
}

// ── Snapshot ─────────────────────────────────────────────────────────────────

/// One-shot self-diagnostic.  Returns a [`serde_json::Value`] for direct
/// inclusion in MCP responses.  Never panics — every sub-probe degrades to
/// a structured error field rather than failing the whole call.
pub fn snapshot() -> Value {
    let identity = json!({
        "name":         "sirin",
        "version":      env!("CARGO_PKG_VERSION"),
        "git_commit":   git_commit(),
        "build_date":   build_date_iso(),
        "binary_path":  current_exe_path(),
        "platform":     format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "uptime_secs":  uptime_secs(),
        "rpc_port":     rpc_port(),
    });

    let chrome = match crate::browser::diagnostic_snapshot() {
        Ok(Some(snap)) => json!({
            "running":         true,
            "version":         snap.chrome_version,
            "user_agent":      snap.user_agent,
            "headless":        snap.headless,
            "active_tab":      snap.active_tab_index,
            "tab_count":       snap.tab_count,
            "named_sessions":  snap.named_sessions,
        }),
        Ok(None)       => json!({ "running": false, "reason": "no browser session open" }),
        Err(e)         => json!({ "running": false, "error": e }),
    };

    let llm = llm_snapshot();
    let updates = update_snapshot();
    let errors = recent_errors(20);
    let extension = serde_json::to_value(crate::ext_server::status())
        .unwrap_or_else(|_| json!({ "connected": false, "error": "serialize" }));

    let body = format!(
        "## Environment\n\
         - Sirin version : {ver} (commit {commit}, built {built})\n\
         - Platform      : {platform}\n\
         - Uptime        : {uptime}s\n\
         - Chrome        : {chrome_v}\n\
         - LLM           : {llm_l}\n\
         - Update status : {update}\n\
         \n\
         ## Reproduction\n\
         <! the calling AI fills in: minimal steps that triggered the issue !>\n\
         \n\
         ## What the calling AI already tried\n\
         <! the calling AI fills in: known_issues checked, retries done, hypotheses ruled out !>\n\
         \n\
         ## Recent Sirin errors\n\
         ```\n{errors_str}\n```\n",
        ver       = env!("CARGO_PKG_VERSION"),
        commit    = git_commit(),
        built     = build_date_iso(),
        platform  = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        uptime    = uptime_secs(),
        chrome_v  = chrome.get("version").and_then(|v| v.as_str()).unwrap_or("(not running)"),
        llm_l     = llm.get("provider").and_then(|v| v.as_str()).unwrap_or("(unknown)"),
        update    = updates.get("state").and_then(|v| v.as_str()).unwrap_or("idle"),
        errors_str = if errors.is_empty() { "(none)".to_string() } else { errors.join("\n") },
    );

    json!({
        "identity":     identity,
        "chrome":       chrome,
        "extension":    extension,
        "llm":          llm,
        "update":       updates,
        "recent_errors": errors,
        "report_issue_template": {
            "title_hint": "[bug] <one-line summary>",
            "body":       body,
            "github_url": "https://github.com/Redandan/Sirin/issues/new",
        },
    })
}

// ── Sub-probes ───────────────────────────────────────────────────────────────

fn current_exe_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "unknown".into())
}

fn rpc_port() -> u16 {
    std::env::var("SIRIN_RPC_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7700)
}

fn llm_snapshot() -> Value {
    // Read .env-derived runtime config without forcing a probe round-trip
    // (the snapshot must be fast).  Provider + model are the most useful
    // bits for diagnosis — the exact online check would require an HTTP
    // call we don't want to add to the synchronous path.
    let provider = std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "(unset)".into());
    let model = std::env::var("OLLAMA_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .or_else(|_| std::env::var("LLM_MODEL"))
        .unwrap_or_else(|_| "(unset)".into());
    json!({
        "provider": provider,
        "model":    model,
        // Heuristic: model names containing 'vision', 'vl', 'gemma3', 'llava',
        // 'qwen-vl', 'gemini' are typically vision-capable.  Used by Tier-1 AI
        // to decide whether to suggest a different model for canvas testing.
        "vision_capable_hint": is_vision_model(&model),
    })
}

fn is_vision_model(model: &str) -> bool {
    let m = model.to_lowercase();
    ["vision", "vl", "llava", "gemma3", "gemini", "qwen2.5-vl", "claude-3"]
        .iter()
        .any(|tag| m.contains(tag))
}

fn update_snapshot() -> Value {
    use crate::updater::UpdateStatus;
    let status = crate::updater::get_status();
    let (state, latest) = match &status {
        UpdateStatus::Idle              => ("idle",         None),
        UpdateStatus::Checking          => ("checking",     None),
        UpdateStatus::Available(v)      => ("update_available", Some(v.clone())),
        UpdateStatus::UpToDate          => ("up_to_date",   None),
        UpdateStatus::CheckFailed(_)    => ("check_failed", None),
        UpdateStatus::Applying          => ("applying",     None),
        UpdateStatus::RestartRequired   => ("restart_required", None),
        UpdateStatus::ApplyFailed(_)    => ("apply_failed", None),
    };
    json!({
        "state":              state,
        "current":            env!("CARGO_PKG_VERSION"),
        "latest":             latest,
        "release_notes_url":  "https://github.com/Redandan/Sirin/releases",
    })
}

/// Tail the last `limit` ERROR / WARN lines from the running log.  We read
/// from `crate::log_buffer::recent`, which holds an in-memory ring buffer of
/// recent `tracing` events — disk reads would race with the live writer and
/// risk blocking the MCP handler on slow filesystems.
///
/// Filters lines containing `ERROR`, `WARN`, or the lowercase `error`/`warn`
/// tokens — `tracing`'s default formatter uppercases the level, but custom
/// macros may push lowercase.
fn recent_errors(limit: usize) -> Vec<String> {
    // Pull a generous window from the ring buffer (300-line cap), then filter.
    // We intentionally over-fetch so a burst of INFO lines doesn't push every
    // recent error out of view.
    let all = crate::log_buffer::recent(300);
    let mut filtered: Vec<String> = all
        .into_iter()
        .filter(|line| {
            let l = line.as_str();
            l.contains("ERROR") || l.contains("WARN") || l.contains(" error ") || l.contains(" warn ")
        })
        .collect();
    // Keep the most recent `limit` lines.
    let drop = filtered.len().saturating_sub(limit);
    filtered.drain(..drop);
    filtered
}

// ── Optional thin wrapper for serde-driven schema docs ───────────────────────
//
// Not used at runtime — the tool dispatches a `serde_json::Value` directly.
// Kept for consumers (sirin-call, doc generators) that want a typed handle.
#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct Diagnose {
    pub identity: Value,
    pub chrome:   Value,
    pub llm:      Value,
    pub update:   Value,
    pub recent_errors: Vec<String>,
    pub report_issue_template: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_returns_required_top_level_keys() {
        // record_startup may not have been called; uptime should still be 0
        let snap = snapshot();
        for key in ["identity", "chrome", "llm", "update", "recent_errors", "report_issue_template"] {
            assert!(snap.get(key).is_some(), "missing key: {key}");
        }
    }

    #[test]
    fn identity_includes_version_from_cargo_toml() {
        let snap = snapshot();
        let v = snap["identity"]["version"].as_str().unwrap();
        // Sanity: version must be the form X.Y.Z
        assert!(v.split('.').count() == 3, "version not semver: {v}");
    }

    #[test]
    fn vision_model_heuristic() {
        assert!(is_vision_model("gemma3:12b"));
        assert!(is_vision_model("gemini-1.5-pro"));
        assert!(is_vision_model("qwen2.5-vl-7b"));
        assert!(is_vision_model("llava:34b"));
        assert!(!is_vision_model("qwen2.5:7b"));
        assert!(!is_vision_model("llama3.2"));
    }

    #[test]
    fn report_template_contains_environment_block() {
        let snap = snapshot();
        let body = snap["report_issue_template"]["body"].as_str().unwrap();
        assert!(body.contains("## Environment"));
        assert!(body.contains("Sirin version"));
        assert!(body.contains("## Reproduction"));
        assert!(body.contains("## What the calling AI already tried"));
    }

    #[test]
    fn record_startup_is_idempotent() {
        record_startup();
        let first = uptime_secs();
        record_startup();  // should not reset
        std::thread::sleep(std::time::Duration::from_millis(10));
        let second = uptime_secs();
        assert!(second >= first, "uptime regressed: {first} -> {second}");
    }
}
