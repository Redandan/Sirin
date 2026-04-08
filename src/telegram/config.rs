//! Telegram configuration — reads all env vars and resolves the session path.
//!
//! Also provides `from_agent_channel` for building a config from a per-agent
//! `TelegramChannelConfig` (stored in `agents.yaml`) instead of env vars.

use std::env;

/// Code + 2FA timeout: how long (seconds) we wait for the user to enter credentials via UI.
pub const AUTH_INPUT_TIMEOUT_SECS: u64 = 300;

/// Resolve a persistent Telegram session path outside the workspace so
/// runtime writes do not trigger Tauri's file watcher.
pub fn session_path() -> std::path::PathBuf {
    if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
        return std::path::Path::new(&local_app_data)
            .join("Sirin")
            .join("sirin.session");
    }

    std::path::Path::new("data").join("sirin.session")
}

/// Whether Telegram sign-in is required at startup.
///
/// Default is optional (`false`) so the desktop app can run without waiting
/// for Telegram credentials. Set `TG_REQUIRE_LOGIN=1` to enforce login.
pub fn require_login() -> bool {
    env::var("TG_REQUIRE_LOGIN")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

pub struct TelegramConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub phone: Option<String>,
    pub auto_reply_enabled: bool,
    pub auto_reply_text: String,
    pub reply_private: bool,
    pub reply_groups: bool,
    /// Group / channel IDs that Sirin should monitor.
    pub group_ids: Vec<i64>,
    /// Message to send to self on startup. None = disabled.
    pub startup_msg: Option<String>,
    /// Optional username target for startup message (e.g. "myuser" or "@myuser").
    pub startup_target: Option<String>,
    /// Emit verbose Telegram update diagnostics.
    pub debug_updates: bool,
}

// ── Env-var reference resolver ────────────────────────────────────────────────

/// Replace every `${VAR_NAME}` placeholder in `s` with the corresponding
/// environment variable value.  Unknown variables are replaced with an empty
/// string.  Literal values (no `${…}`) are returned unchanged.
pub fn resolve_env_refs(s: &str) -> String {
    let mut result = s.to_string();
    loop {
        let Some(start) = result.find("${") else { break };
        let Some(rel_end) = result[start..].find('}') else { break };
        let end = start + rel_end;
        let var_name = &result[start + 2..end];
        let value = env::var(var_name).unwrap_or_default();
        result = format!("{}{}{}", &result[..start], value, &result[end + 1..]);
    }
    result
}

// ── Session path ──────────────────────────────────────────────────────────────

/// Resolve a session file path from an agent config value.
///
/// - Empty string → use the system default (`session_path()`).
/// - Any string with `${VAR}` placeholders → resolve them first.
pub fn resolve_session_path(configured: &str) -> std::path::PathBuf {
    if configured.trim().is_empty() {
        return session_path();
    }
    std::path::PathBuf::from(resolve_env_refs(configured))
}

// ── Config constructors ───────────────────────────────────────────────────────

impl TelegramConfig {
    /// Read configuration from environment variables.
    ///
    /// Returns an error when any required variable is absent or malformed.
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let api_id: i32 = env::var("TG_API_ID")
            .map_err(|_| "TG_API_ID not set in environment")?
            .trim()
            .parse()
            .map_err(|e| format!("TG_API_ID must be an integer: {e}"))?;

        let api_hash = env::var("TG_API_HASH").map_err(|_| "TG_API_HASH not set in environment")?;
        let phone = env::var("TG_PHONE").ok().and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        let auto_reply_enabled = env::var("TG_AUTO_REPLY")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

        let auto_reply_text = env::var("TG_AUTO_REPLY_TEXT")
            .ok()
            .and_then(|v| {
                let t = v.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            })
            .unwrap_or_else(|| "{ack_prefix} 我會先幫你處理這件事。".to_string());

        let reply_private = env::var("TG_REPLY_PRIVATE")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(true);

        let reply_groups = env::var("TG_REPLY_GROUPS")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

        let group_ids: Vec<i64> = env::var("TG_GROUP_IDS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    trimmed.parse::<i64>().ok()
                }
            })
            .collect();

        let startup_msg = match env::var("TG_STARTUP_MSG") {
            Ok(v) => {
                let t = v.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            }
            // Default to enabled, so startup health is visible even without .env loading.
            Err(_) => Some("Sirin started at {time}".to_string()),
        };

        let startup_target_raw = env::var("TG_STARTUP_TARGET");
        eprintln!(
            "[telegram] TG_STARTUP_TARGET env = {:?}",
            startup_target_raw
        );
        let startup_target = startup_target_raw.ok().and_then(|v| {
            let t = v.trim().trim_start_matches('@').to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });

        let debug_updates = env::var("TG_DEBUG_UPDATES")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(true);

        Ok(Self {
            api_id,
            api_hash,
            phone,
            auto_reply_enabled,
            auto_reply_text,
            reply_private,
            reply_groups,
            group_ids,
            startup_msg,
            startup_target,
            debug_updates,
        })
    }

    /// Build a `TelegramConfig` from a per-agent channel config.
    ///
    /// `${VAR}` references in string fields are resolved at call time so that
    /// agents can store references to env vars (e.g. `"${TG_API_ID}"`) without
    /// hard-coding credentials in `agents.yaml`.
    pub fn from_agent_channel(
        ch: &crate::agent_config::TelegramChannelConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let api_id_str = resolve_env_refs(&ch.api_id);
        let api_id: i32 = api_id_str
            .trim()
            .parse()
            .map_err(|e| format!("agent channel api_id '{}' is not an integer: {e}", api_id_str))?;

        let api_hash = resolve_env_refs(&ch.api_hash);
        if api_hash.trim().is_empty() || api_hash.starts_with("${") {
            return Err("agent channel api_hash not resolved (env var missing?)".into());
        }

        let phone_raw = resolve_env_refs(&ch.phone);
        let phone = if phone_raw.trim().is_empty() || phone_raw.starts_with("${") {
            None
        } else {
            Some(phone_raw.trim().to_string())
        };

        Ok(Self {
            api_id,
            api_hash,
            phone,
            auto_reply_enabled: ch.auto_reply,
            auto_reply_text: "{ack_prefix} 我會先幫你處理這件事。".to_string(),
            reply_private: ch.reply_private,
            reply_groups: ch.reply_groups,
            group_ids: ch.group_ids.clone(),
            startup_msg: ch.startup_msg.clone(),
            startup_target: None,
            debug_updates: true,
        })
    }
}
