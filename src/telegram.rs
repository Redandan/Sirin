//! Telegram listener module for Sirin.
//!
//! Connects to the Telegram MTProto API via the `grammers` crate, monitors
//! a configurable set of group chats, and fires a macOS system notification
//! whenever a message signals high ROI potential according to the active
//! [`Persona`] configuration.
//!
//! ## Required environment variables (loaded from `.env`)
//!
//! | Variable        | Description                                      |
//! |-----------------|--------------------------------------------------|
//! | `TG_API_ID`     | Integer App API ID from <https://my.telegram.org> |
//! | `TG_API_HASH`   | Hex App API hash from <https://my.telegram.org>  |
//! | `TG_GROUP_IDS`  | Comma-separated list of group/chat IDs to watch  |
//!
//! The Telegram session is stored in `data/sirin.session` so that re-runs do
//! not require re-authentication.

use std::io::{self, Write};
use std::{env, sync::Arc};

use grammers_client::client::UpdatesConfiguration;
use grammers_client::peer::Peer;
use grammers_client::SignInError;
use grammers_client::{Client, SenderPool};
use grammers_client::update::Update;
use grammers_session::storages::SqliteSession;

use crate::persona::{Persona, TaskEntry, TaskTracker};

/// Maximum number of characters to include in a notification preview body.
const NOTIFICATION_PREVIEW_LEN: usize = 120;

/// Resolve a persistent Telegram session path outside the workspace so
/// runtime writes do not trigger Tauri's file watcher.
fn session_path() -> std::path::PathBuf {
    if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
        return std::path::Path::new(&local_app_data)
            .join("Sirin")
            .join("sirin.session");
    }

    std::path::Path::new("data").join("sirin.session")
}

struct TelegramConfig {
    api_id: i32,
    api_hash: String,
    phone: Option<String>,
    auto_reply_enabled: bool,
    auto_reply_text: String,
    reply_private: bool,
    reply_groups: bool,
    /// Group / channel IDs that Sirin should monitor.
    group_ids: Vec<i64>,
}

impl TelegramConfig {
    /// Read configuration from environment variables.
    ///
    /// Returns an error when any required variable is absent or malformed.
    fn from_env() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let api_id: i32 = env::var("TG_API_ID")
            .map_err(|_| "TG_API_ID not set in environment")?
            .trim()
            .parse()
            .map_err(|e| format!("TG_API_ID must be an integer: {e}"))?;

        let api_hash = env::var("TG_API_HASH")
            .map_err(|_| "TG_API_HASH not set in environment")?;
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
            .unwrap_or_else(|| "已收到你的訊息，Sirin 正在分析，稍後回覆。".to_string());

        let reply_private = env::var("TG_REPLY_PRIVATE")
            .ok()
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);

        let reply_groups = env::var("TG_REPLY_GROUPS")
            .ok()
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);

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

        Ok(Self {
            api_id,
            api_hash,
            phone,
            auto_reply_enabled,
            auto_reply_text,
            reply_private,
            reply_groups,
            group_ids,
        })
    }
}

fn prompt_input(prompt: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

async fn ensure_user_authorized(
    client: &Client,
    cfg: &TelegramConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if client.is_authorized().await? {
        return Ok(());
    }

    eprintln!("[telegram] Session not authorized, starting user sign-in flow...");

    let phone = if let Some(phone) = cfg.phone.as_ref() {
        phone.clone()
    } else {
        prompt_input("Enter Telegram phone number (international format, e.g. +886...): ")?
    };

    let login_token = client.request_login_code(&phone, &cfg.api_hash).await?;
    let code = prompt_input("Enter Telegram login code: ")?;

    match client.sign_in(&login_token, &code).await {
        Ok(_) => {
            eprintln!("[telegram] User sign-in succeeded");
            Ok(())
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            let hint = password_token.hint().unwrap_or("none");
            let password = prompt_input(&format!("Enter Telegram 2FA password (hint: {hint}): "))?;
            client.check_password(password_token, password.trim()).await?;
            eprintln!("[telegram] User sign-in with 2FA succeeded");
            Ok(())
        }
        Err(err) => Err(format!("Telegram sign-in failed: {err}").into()),
    }
}

/// Estimate the ROI potential of a Telegram message.
///
/// This is intentionally a simple heuristic — it assigns a nominal profit
/// value based on the presence of trading-related keywords so the
/// [`Persona::should_trigger_remote_ai`] gate can decide whether to escalate.
///
/// Replace or extend this logic with a real ML inference call when available.
fn estimate_profit(text: &str) -> f64 {
    let lower = text.to_lowercase();
    let keywords: &[(&str, f64)] = &[
        // Action words — direct buy/sell intent (moderate weight)
        ("buy",       3.0),
        ("sell",      3.0),
        // Signal / alert words — explicit trading callout (higher weight)
        ("signal",    4.0),
        ("alert",     2.0),
        ("breakout",  6.0),
        // Sentiment / slang common in crypto trading groups
        ("pump",      5.0),
        ("moon",      5.0),
        // Generic profit / trade references
        ("profit",    4.0),
        ("roi",       4.0),
        ("trade",     3.0),
    ];
    keywords.iter().fold(0.0, |acc, (kw, score)| {
        if lower.contains(kw) { acc + score } else { acc }
    })
}

/// Send a macOS (or desktop) system notification.
fn send_notification(title: &str, body: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .show()
    {
        eprintln!("[telegram] Failed to send notification: {e}");
    }
}

/// Connect to Telegram and listen for messages in the configured groups.
///
/// This function runs indefinitely.  Spawn it with [`tokio::spawn`] so it
/// runs alongside the rest of the Sirin background tasks.
///
/// # Errors
/// Returns early with an error if:
/// - required environment variables are missing / malformed,
/// - the Telegram connection cannot be established, or
/// - an unrecoverable network error occurs.
pub async fn run_listener(
    tracker: TaskTracker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cfg = TelegramConfig::from_env()?;

    let session_path = session_path();
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let session = Arc::new(SqliteSession::open(session_path).await?);
    let SenderPool {
        runner,
        updates,
        handle,
    } = SenderPool::new(Arc::clone(&session), cfg.api_id);
    let client = Client::new(handle.clone());
    let pool_task = tokio::spawn(runner.run());

    if let Err(err) = ensure_user_authorized(&client, &cfg).await {
        eprintln!("[telegram] {err}");
        handle.quit();
        let _ = pool_task.await;
        return Ok(());
    }

    let mut updates = client
        .stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: true,
                ..Default::default()
            },
        )
        .await;

    eprintln!("[telegram] Connected to Telegram");

    loop {
        let update = match updates.next().await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[telegram] Error receiving update: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let Update::NewMessage(message) = update else {
            continue;
        };

        if message.outgoing() {
            continue;
        }

        let is_private = matches!(message.peer(), Some(Peer::User(_)));
        if is_private && !cfg.reply_private {
            continue;
        }

        if !is_private && !cfg.reply_groups {
            continue;
        }

        // TG_GROUP_IDS only applies to group/channel chats.
        if !is_private && !cfg.group_ids.is_empty() {
            let peer_id = message.peer_id().bare_id();
            if !cfg.group_ids.contains(&peer_id) {
                continue;
            }
        }

        let text = message.text().to_owned();
        if text.is_empty() {
            continue;
        }

        let persona = match Persona::load() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[telegram] Could not load persona: {e}");
                continue;
            }
        };

        let estimated_profit = estimate_profit(&text);
        let triggered = persona.should_trigger_remote_ai(estimated_profit);

        if cfg.auto_reply_enabled {
            let reply = cfg
                .auto_reply_text
                .replace("{persona}", &persona.name)
                .replace("{profit}", &format!("{estimated_profit:.2}"));

            if let Err(e) = message.reply(reply).await {
                eprintln!("[telegram] Failed to auto-reply: {e}");
            } else {
                eprintln!("[telegram] Auto-reply sent");
            }
        }

        let entry = TaskEntry::ai_decision(&persona.name, estimated_profit, triggered);
        if let Err(e) = tracker.record(&entry) {
            eprintln!("[telegram] Failed to record task entry: {e}");
        }

        if triggered {
            let preview: String = text.chars().take(NOTIFICATION_PREVIEW_LEN).collect();
            send_notification(
                "Sirin — High ROI Signal",
                &format!("Estimated profit: ${estimated_profit:.2}\n\n{preview}"),
            );
            eprintln!(
                "[telegram] ROI trigger fired (profit={estimated_profit:.2}, persona={})",
                persona.name
            );
        }
    }
}
