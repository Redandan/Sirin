/// Pre-Authorization Engine — public API entry point.
///
/// # Actual implementation (mcp_server.rs § call_browser_exec)
///
/// The Monitor GUI integration is fully implemented in `mcp_server.rs`
/// (lines 1505–1556). When a `Decision::Ask` or `Decision::AskWithLearn`
/// is returned:
///
/// 1. `emit_authz_ask(request_id, client, action, args, url, timeout, learn_flag)`
///    → posts event to Monitor → appears in authz_modal.rs UI
/// 2. `register_authz_ask(request_id)` → returns oneshot receiver
/// 3. `tokio::time::timeout(30s, rx)` → waits for human Allow/Deny
/// 4. User clicks Allow/Deny in UI → `resolve_authz_ask()` sends decision
/// 5. Handler resumes action execution or rejects with error
///
/// If Monitor is not initialized, authz asks fail with "no monitor GUI" error.
///
/// ```rust,ignore
/// use crate::authz::{AuthzConfig, Decision, decide, audit};
///
/// let cfg = authz::global_config();
/// let decision = authz::decide(
///     &client_id, &action, &args, &browser::current_url(), &cfg,
/// );
/// match &decision {
///     Decision::Allow(reason) => {
///         audit::log_allow(&cfg.audit.log_path, &client_id, &action, &args, &url, reason);
///     }
///     Decision::Deny(reason) => {
///         audit::log_deny(&cfg.audit.log_path, &client_id, &action, &args, &url, reason);
///         return mcp_error(format!("authz deny: {reason}"), ...);
///     }
///     Decision::Ask(reason) | Decision::AskWithLearn => {
///         // Emit to Monitor UI; wait for human decision (30s timeout)
///         let req_id = format!("ask-{}-{}", &action, uuid());
///         emit_authz_ask(&req_id, &client_id, &action, &args, &url, 30_000, learn_flag).await;
///         let rx = monitor_state.register_authz_ask(&req_id);
///         match tokio::time::timeout(Duration::from_secs(30), rx).await {
///             Ok(Ok(AuthzDecisionResult::Allow)) => {}, // proceed
///             _ => return mcp_error("authz ask denied or timed out", ...),
///         }
///     }
/// }
/// ```
pub mod audit;
pub mod config;
pub mod engine;
pub mod url_blocker;

// Re-export the most-used public surface.
// These will be used by mcp_server.rs in T4; suppress unused-import lint until then.
#[allow(unused_imports)]
pub use config::{AuthzConfig, Mode, Rule};
#[allow(unused_imports)]
pub use engine::{decide, Decision};
#[allow(unused_imports)]
pub use url_blocker::UrlBlocker;

use std::sync::Mutex;

// ─── Global config ────────────────────────────────────────────────────────────

/// Process-wide loaded config.
/// Initialized lazily on first call to `global_config()` or `init()`.
static GLOBAL_CONFIG: Mutex<Option<AuthzConfig>> = Mutex::new(None);

/// Return a clone of the process-wide `AuthzConfig`.
///
/// If `init()` has not been called yet, loads with `repo_root = None`
/// (built-in defaults + user-global only).
pub fn global_config() -> AuthzConfig {
    let guard = GLOBAL_CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = guard.as_ref() {
        return cfg.clone();
    }
    drop(guard);
    // Lazy-init with defaults
    init(None);
    GLOBAL_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .cloned()
        .unwrap_or_else(config::defaults)
}

/// Initialize (or reload) the process-wide config from the given repo root.
///
/// Typically called once at startup from `main()` with the current working dir.
pub fn init(repo_root: Option<&std::path::Path>) {
    let mut cfg = config::load(repo_root);
    // Env var override / merge: SIRIN_BLOCKED_URL_PATTERNS=p1,p2,p3
    if let Ok(raw) = std::env::var("SIRIN_BLOCKED_URL_PATTERNS") {
        for p in raw.split(',') {
            let p = p.trim();
            if !p.is_empty() && !cfg.blocked_url_patterns.contains(&p.to_string()) {
                cfg.blocked_url_patterns.push(p.to_string());
            }
        }
    }
    let mut guard = GLOBAL_CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(cfg);
}

// ─── URL blocker (Issue #81) ─────────────────────────────────────────────────

/// Check `url` against the process-wide blocklist + any extra per-call
/// patterns (e.g. from a YAML test goal's `blocked_url_patterns`).
///
/// Returns `Some(matched_pattern)` if blocked, `None` if allowed.
///
/// This is the cheap-path entry used by the `web_navigate` tool's `goto`
/// branch — it spends zero CDP calls and short-circuits before the
/// browser ever sees the URL.
pub fn check_blocked_url<'a, I, S>(url: &str, extra_patterns: I) -> Option<String>
where
    I: IntoIterator<Item = &'a S>,
    S: AsRef<str> + 'a,
{
    let cfg = global_config();
    // Process-wide list first.
    let blocker = UrlBlocker::from_patterns(cfg.blocked_url_patterns.iter());
    if let Some(p) = blocker.check(url) {
        return Some(p.to_string());
    }
    // Per-call extra patterns (e.g. test-goal scoped).
    let extra: Vec<String> = extra_patterns
        .into_iter()
        .map(|s| s.as_ref().to_string())
        .collect();
    if !extra.is_empty() {
        let blocker_extra = UrlBlocker::from_patterns(extra.iter());
        if let Some(p) = blocker_extra.check(url) {
            return Some(p.to_string());
        }
    }
    None
}
