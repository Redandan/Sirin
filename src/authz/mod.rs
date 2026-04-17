/// Pre-Authorization Engine — public API entry point.
///
/// Usage from `mcp_server.rs` (T4):
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
///     Decision::Ask(_) | Decision::AskWithLearn => {
///         // TODO T4: wire to Monitor GUI; for now treat as deny
///         audit::log_ask(&cfg.audit.log_path, &client_id, &action, &args, &url, "ask→deny(no gui)");
///         return mcp_error("authz ask — no GUI attached, treated as deny", ...);
///     }
/// }
/// ```

pub mod audit;
pub mod config;
pub mod engine;

// Re-export the most-used public surface.
// These will be used by mcp_server.rs in T4; suppress unused-import lint until then.
#[allow(unused_imports)]
pub use config::{AuthzConfig, Mode, Rule};
#[allow(unused_imports)]
pub use engine::{decide, Decision};

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
    let cfg = config::load(repo_root);
    let mut guard = GLOBAL_CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(cfg);
}
