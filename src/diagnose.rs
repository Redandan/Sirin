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

/// Whether the source tree contained tracked or untracked changes when this
/// binary was built. `None` means git was unavailable, so callers must not
/// assume the build came from a clean checkout.
pub fn git_dirty() -> Option<bool> {
    match option_env!("SIRIN_GIT_DIRTY") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// Human-readable build identity that never hides an unknown or dirty source
/// state behind an otherwise valid commit SHA.
pub fn build_identity() -> String {
    match git_dirty() {
        Some(true) => format!("{}-dirty", git_commit()),
        Some(false) => git_commit().to_string(),
        None => format!("{}-source-state-unknown", git_commit()),
    }
}

/// Unix epoch seconds at build time.  Used to derive `build_date` in the
/// snapshot; we keep it as a number here so the runtime side can format it
/// however it pleases (chrono is a runtime dep, not a build-script dep).
pub fn build_epoch() -> u64 {
    option_env!("SIRIN_BUILD_EPOCH")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn build_date_iso() -> String {
    let secs = build_epoch();
    if secs == 0 {
        return "unknown".into();
    }
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
        "git_dirty":    git_dirty(),
        "build_identity": build_identity(),
        "build_date":   build_date_iso(),
        "binary_path":  current_exe_path(),
        "platform":     format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "uptime_secs":  uptime_secs(),
        "rpc_port":     rpc_port(),
    });

    let chrome = match crate::browser::diagnostic_snapshot() {
        Ok(Some(snap)) => {
            // `chrome_version` is populated only when `Browser.getVersion` succeeds.
            // If it's None, the CDP transport is dead even though we still hold the
            // `Browser` handle — to a caller, an unresponsive Chrome is the same
            // as no Chrome.  Report `running: false` honestly so the calling AI
            // can suggest "relaunch Sirin" instead of chasing a phantom session.
            let responsive = snap.chrome_version.is_some();
            json!({
                "running":         responsive,
                "responsive":      responsive,
                "session_held":    true,
                "version":         snap.chrome_version,
                "user_agent":      snap.user_agent,
                "headless":        snap.headless,
                "active_tab":      snap.active_tab_index,
                "tab_count":       snap.tab_count,
                "named_sessions":  snap.named_sessions,
                "reason":          if responsive { Value::Null }
                                   else { json!("session_held but CDP unresponsive — Chrome likely crashed") },
            })
        }
        Ok(None) => {
            json!({ "running": false, "session_held": false, "reason": "no browser session open" })
        }
        Err(e) => json!({ "running": false, "session_held": false, "error": e }),
    };

    let llm = llm_snapshot();
    let updates = update_snapshot();
    let errors = recent_errors(20);
    let extension = serde_json::to_value(crate::ext_server::status())
        .unwrap_or_else(|_| json!({ "connected": false, "error": "serialize" }));

    let platform_str = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
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
        ver = env!("CARGO_PKG_VERSION"),
        commit = build_identity(),
        built = build_date_iso(),
        platform = platform_str,
        uptime = uptime_secs(),
        chrome_v = chrome
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("(not running)"),
        llm_l = format!(
            "{}/{}",
            llm.get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)"),
            llm.get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)")
        ),
        update = updates
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("idle"),
        errors_str = if errors.is_empty() {
            "(none)".to_string()
        } else {
            errors.join("\n")
        },
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
    // Report the process-wide effective config, not a second reconstruction
    // from env vars. Startup probing and llm.yaml overrides may select a
    // different provider/model than the raw environment contains.
    let llm = crate::llm::shared_llm();
    let provider = llm.backend_name();
    let model = llm.model.clone();

    let reachability = llm_reachability_cached();

    json!({
        "provider": provider,
        "model":    model,
        "vision_capable_hint": llm.supports_vision_hint(),
        // Cheap reachability check — `null` if never probed, `true`/`false`
        // otherwise.  Updated by background task every ~30s.
        "reachable":            reachability.reachable,
        "reachability_checked_at_secs_ago": reachability.checked_at_secs_ago,
        "reachability_error":   reachability.error,
    })
}

// ── LLM reachability cache ──────────────────────────────────────────────────
//
// We don't want `diagnose` to block on an HTTP round-trip — the snapshot is
// called from MCP handlers and must stay fast.  Instead, [`spawn_reachability_probe`]
// runs a background task that pings the configured backend every 30s and
// caches the result.  `llm_snapshot()` reads the cache.

#[derive(Clone, Default)]
struct LlmReachability {
    reachable: Option<bool>,
    checked_at: Option<Instant>,
    error: Option<String>,
}

#[derive(Serialize, Default)]
struct LlmReachabilityView {
    reachable: Option<bool>,
    checked_at_secs_ago: Option<u64>,
    error: Option<String>,
}

static LLM_REACH: OnceLock<std::sync::Mutex<LlmReachability>> = OnceLock::new();

fn llm_reach_cell() -> &'static std::sync::Mutex<LlmReachability> {
    LLM_REACH.get_or_init(|| std::sync::Mutex::new(LlmReachability::default()))
}

fn llm_reachability_cached() -> LlmReachabilityView {
    let g = llm_reach_cell().lock().unwrap_or_else(|e| e.into_inner());
    LlmReachabilityView {
        reachable: g.reachable,
        checked_at_secs_ago: g.checked_at.map(|t| t.elapsed().as_secs()),
        error: g.error.clone(),
    }
}

/// Spawn a background task that probes the configured LLM backend every 30s
/// and caches the result for [`llm_snapshot`].  Idempotent — first caller wins.
///
/// The probe is intentionally cheap: a `GET` to the backend's models/health
/// endpoint with a 2s timeout.  We never block the diagnose call itself.
pub fn spawn_reachability_probe() {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return; // already spawned
    }
    tokio::spawn(async move {
        loop {
            let (reachable, err) = probe_llm_once().await;
            {
                let mut g = llm_reach_cell().lock().unwrap_or_else(|e| e.into_inner());
                g.reachable = Some(reachable);
                g.checked_at = Some(Instant::now());
                g.error = err;
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

async fn probe_llm_once() -> (bool, Option<String>) {
    let llm = crate::llm::shared_llm();

    let url = match llm.backend {
        crate::llm::LlmBackend::LmStudio => {
            let base = llm.base_url.clone();
            let client = reqwest::Client::new();
            if let Err(e) =
                crate::llm::ensure_lmstudio_server(&client, &base, llm.api_key.as_deref()).await
            {
                crate::sirin_log!("[diagnose] LM Studio auto-start failed: {e}");
            }
            format!("{}/models", base.trim_end_matches('/'))
        }
        crate::llm::LlmBackend::Gemini => {
            // Gemini doesn't expose a cheap unauthenticated health endpoint;
            // probing models requires the API key.  Skip — return reachable=true
            // so we don't false-flag working remote configs.
            return (
                true,
                Some("(skipped: gemini has no anonymous health endpoint)".to_string()),
            );
        }
        crate::llm::LlmBackend::Anthropic => {
            return (
                true,
                Some("(skipped: anthropic has no anonymous health endpoint)".to_string()),
            );
        }
        crate::llm::LlmBackend::Ollama => {
            let base = llm.base_url.clone();
            format!("{}/api/tags", base.trim_end_matches('/'))
        }
    };

    let client = reqwest::Client::new();
    match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => (true, None),
        Ok(r) => (false, Some(format!("HTTP {}", r.status()))),
        Err(e) => (false, Some(e.to_string())),
    }
}

fn update_snapshot() -> Value {
    let status = crate::updater::get_status();
    let (state, latest, error) = update_status_fields(&status);
    json!({
        "state":              state,
        "current":            env!("CARGO_PKG_VERSION"),
        "latest":             latest,
        "error":              error,
        "release_notes_url":  "https://github.com/Redandan/Sirin/releases",
    })
}

fn update_status_fields(
    status: &crate::updater::UpdateStatus,
) -> (&'static str, Option<String>, Option<String>) {
    use crate::updater::UpdateStatus;
    match status {
        UpdateStatus::Idle => ("idle", None, None),
        UpdateStatus::Checking => ("checking", None, None),
        UpdateStatus::Available(version) => ("update_available", Some(version.clone()), None),
        UpdateStatus::UpToDate => ("up_to_date", None, None),
        UpdateStatus::CheckFailed(error) => ("check_failed", None, Some(error.clone())),
        UpdateStatus::Applying => ("applying", None, None),
        UpdateStatus::RestartRequired => ("restart_required", None, None),
        UpdateStatus::ApplyFailed(error) => ("apply_failed", None, Some(error.clone())),
    }
}

/// Tail the last `limit` ERROR / WARN lines from the running log.  We read
/// from `crate::log_buffer::recent`, which holds an in-memory ring buffer of
/// recent `tracing` events — disk reads would race with the live writer and
/// risk blocking the MCP handler on slow filesystems.
///
/// Matches the exact `[ERROR] ` / `[WARN] ` prefix that
/// [`crate::log_subscriber::LogBufferLayer`] prepends.  Substring-matching
/// `"ERROR"` anywhere in the line was too loose — a perfectly fine status log
/// like `"no ERROR detected in last build"` would have leaked into the report.
fn recent_errors(limit: usize) -> Vec<String> {
    // Pull a generous window from the ring buffer (300-line cap), then filter.
    // We intentionally over-fetch so a burst of INFO lines doesn't push every
    // recent error out of view.
    let all = crate::log_buffer::recent(300);
    let mut filtered: Vec<String> = all
        .into_iter()
        .filter(|line| {
            // Prefix is what LogBufferLayer guarantees; checking for the
            // bracketed token anywhere in the line still catches eprintln!
            // style output ("[ERROR] foo") from legacy paths we haven't
            // yet migrated to tracing.
            line.starts_with("[ERROR]")
                || line.starts_with("[WARN]")
                || line.contains(" [ERROR] ")
                || line.contains(" [WARN] ")
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
    pub chrome: Value,
    pub llm: Value,
    pub update: Value,
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
        for key in [
            "identity",
            "chrome",
            "llm",
            "update",
            "recent_errors",
            "report_issue_template",
        ] {
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
    fn identity_never_hides_dirty_or_unknown_source_state() {
        let snap = snapshot();
        let identity = &snap["identity"];
        let build_identity = identity["build_identity"].as_str().unwrap();
        assert!(build_identity.starts_with(git_commit()));

        match git_dirty() {
            Some(true) => {
                assert_eq!(identity["git_dirty"], Value::Bool(true));
                assert!(build_identity.ends_with("-dirty"));
            }
            Some(false) => {
                assert_eq!(identity["git_dirty"], Value::Bool(false));
                assert_eq!(build_identity, git_commit());
            }
            None => {
                assert!(identity["git_dirty"].is_null());
                assert!(build_identity.ends_with("-source-state-unknown"));
            }
        }
    }

    #[test]
    fn llm_snapshot_matches_the_effective_runtime_config() {
        let effective = crate::llm::shared_llm();
        let snap = llm_snapshot();
        assert_eq!(snap["provider"], effective.backend_name());
        assert_eq!(snap["model"], effective.model);
        assert_eq!(
            snap["vision_capable_hint"],
            effective.supports_vision_hint()
        );
    }

    #[test]
    fn update_failures_keep_the_actionable_error() {
        let (state, latest, error) = update_status_fields(
            &crate::updater::UpdateStatus::CheckFailed("network timeout".to_string()),
        );
        assert_eq!(state, "check_failed");
        assert_eq!(latest, None);
        assert_eq!(error.as_deref(), Some("network timeout"));
    }

    #[test]
    fn report_template_contains_environment_block() {
        let snap = snapshot();
        let body = snap["report_issue_template"]["body"].as_str().unwrap();
        let effective = crate::llm::shared_llm();
        assert!(body.contains("## Environment"));
        assert!(body.contains("Sirin version"));
        assert!(body.contains(&format!("{}/{}", effective.backend_name(), effective.model)));
        assert!(body.contains("## Reproduction"));
        assert!(body.contains("## What the calling AI already tried"));
    }

    #[test]
    fn record_startup_is_idempotent() {
        record_startup();
        let first = uptime_secs();
        record_startup(); // should not reset
        std::thread::sleep(std::time::Duration::from_millis(10));
        let second = uptime_secs();
        assert!(second >= first, "uptime regressed: {first} -> {second}");
    }

    #[test]
    fn recent_errors_filter_matches_bracketed_prefix_only() {
        // Push known lines and verify only properly-tagged ones come through.
        // We filter by `[ERROR]` / `[WARN]` prefix so benign status lines
        // mentioning the word "ERROR" don't leak into bug reports.
        crate::log_buffer::clear();
        crate::log_buffer::push("[ERROR] real error here".to_string());
        crate::log_buffer::push("info: no ERROR detected in last build".to_string());
        crate::log_buffer::push("[WARN] real warning here".to_string());
        crate::log_buffer::push("info: warning shown to user".to_string());
        crate::log_buffer::push("plain info line".to_string());

        let errors = recent_errors(20);
        assert!(
            errors.iter().any(|l| l == "[ERROR] real error here"),
            "missing [ERROR] line, got: {errors:?}"
        );
        assert!(
            errors.iter().any(|l| l == "[WARN] real warning here"),
            "missing [WARN] line, got: {errors:?}"
        );
        assert!(
            !errors.iter().any(|l| l.contains("no ERROR detected")),
            "leaked benign ERROR mention: {errors:?}"
        );
        assert!(
            !errors.iter().any(|l| l.contains("warning shown to user")),
            "leaked benign warning mention: {errors:?}"
        );
        assert!(
            !errors.iter().any(|l| l == "plain info line"),
            "leaked plain info: {errors:?}"
        );
    }

    #[test]
    fn llm_reachability_starts_unknown_until_probed() {
        // Without spawn_reachability_probe(), the cache is empty — fields
        // should be None / null rather than panicking.  In the snapshot JSON,
        // serde_json represents Option::None as serde_json::Value::Null.
        let view = llm_reachability_cached();
        // We can't guarantee `reachable.is_none()` — another test in this
        // process may have called the probe — but the structure must serialize.
        let v = serde_json::to_value(&view).expect("LlmReachabilityView must serialize");
        assert!(v.get("reachable").is_some());
        assert!(v.get("checked_at_secs_ago").is_some());
        assert!(v.get("error").is_some());
    }

    #[test]
    fn chrome_snapshot_unresponsive_when_no_browser() {
        // No browser session in unit-test context — should report not running.
        let snap = snapshot();
        let chrome = &snap["chrome"];
        // session_held = false because we never opened a browser
        assert_eq!(
            chrome["session_held"],
            serde_json::Value::Bool(false),
            "expected session_held=false in unit test, got {chrome}"
        );
        assert_eq!(
            chrome["running"],
            serde_json::Value::Bool(false),
            "expected running=false when no session, got {chrome}"
        );
    }
}
