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
use std::collections::HashMap;

use chrono::Utc;
use grammers_client::client::UpdatesConfiguration;
use grammers_client::SignInError;
use grammers_client::{Client, SenderPool};
use grammers_client::update::Update;
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerId, PeerKind};
use serde::{Deserialize, Serialize};

use crate::persona::{Persona, TaskEntry, TaskTracker};

/// Maximum number of characters to include in a notification preview body.
const NOTIFICATION_PREVIEW_LEN: usize = 120;
const OLLAMA_BASE_URL: &str = "http://localhost:11434";
const LM_STUDIO_BASE_URL: &str = "http://localhost:1234/v1";
const DEFAULT_MODEL: &str = "llama3.2";

#[derive(Debug, Clone, Copy)]
enum ReplyLlmBackend {
    Ollama,
    LmStudio,
}

#[derive(Debug, Clone)]
struct ReplyLlmConfig {
    backend: ReplyLlmBackend,
    base_url: String,
    model: String,
    api_key: Option<String>,
}

impl ReplyLlmConfig {
    fn from_env() -> Self {
        let provider = std::env::var("LLM_PROVIDER")
            .unwrap_or_else(|_| "ollama".to_string())
            .to_lowercase();

        match provider.as_str() {
            "lmstudio" | "lm_studio" | "openai" => Self {
                backend: ReplyLlmBackend::LmStudio,
                base_url: std::env::var("LM_STUDIO_BASE_URL")
                    .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                    .unwrap_or_else(|_| LM_STUDIO_BASE_URL.to_string()),
                model: std::env::var("LM_STUDIO_MODEL")
                    .or_else(|_| std::env::var("OPENAI_MODEL"))
                    .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
                api_key: std::env::var("LM_STUDIO_API_KEY")
                    .or_else(|_| std::env::var("OPENAI_API_KEY"))
                    .ok()
                    .filter(|v| !v.trim().is_empty()),
            },
            _ => Self {
                backend: ReplyLlmBackend::Ollama,
                base_url: std::env::var("OLLAMA_BASE_URL")
                    .unwrap_or_else(|_| OLLAMA_BASE_URL.to_string()),
                model: std::env::var("OLLAMA_MODEL")
                    .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
                api_key: None,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    prompt: String,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    response: String,
}

#[derive(Debug, Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage<'a>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct OpenAiMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoiceMessage {
    content: String,
}

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
    /// Message to send to self on startup. None = disabled.
    startup_msg: Option<String>,
    /// Optional username target for startup message (e.g. "myuser" or "@myuser").
    startup_target: Option<String>,
    /// Emit verbose Telegram update diagnostics.
    debug_updates: bool,
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
            .unwrap_or_else(|| {
                "{ack_prefix} 我是{persona}，會用{voice}的語氣回覆你。{compliance}（估計收益 {profit} USD）"
                    .to_string()
            });

        let reply_private = env::var("TG_REPLY_PRIVATE")
            .ok()
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);

        let reply_groups = env::var("TG_REPLY_GROUPS")
            .ok()
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
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
        eprintln!("[telegram] TG_STARTUP_TARGET env = {:?}", startup_target_raw);
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
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
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

/// Execute simple user commands from Telegram message text and return
/// a human-readable execution report.
fn execute_user_request(
    text: &str,
    tracker: &TaskTracker,
    persona_name: &str,
) -> Option<String> {
    let normalized = text.trim();
    if normalized.is_empty() {
        return None;
    }

    let lower = normalized.to_lowercase();

    // 1) Create a pending task from explicit user instruction.
    if lower.starts_with("todo ")
        || normalized.starts_with("待辦")
        || normalized.starts_with("記錄任務")
        || normalized.starts_with("幫我記錄")
    {
        let detail = normalized
            .trim_start_matches("todo")
            .trim_start_matches('：')
            .trim_start_matches(':')
            .trim();

        let entry = TaskEntry {
            timestamp: Utc::now().to_rfc3339(),
            event: "user_request".to_string(),
            persona: persona_name.to_string(),
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: Some("PENDING".to_string()),
            reason: Some(if detail.is_empty() {
                normalized.to_string()
            } else {
                detail.to_string()
            }),
            action_tier: None,
            high_priority: None,
        };

        return match tracker.record(&entry) {
            Ok(_) => Some("執行結果：已幫你建立待辦，狀態為 PENDING。".to_string()),
            Err(e) => Some(format!("執行結果：建立待辦失敗，原因：{e}")),
        };
    }

    // 2) Query actionable tasks.
    if normalized.contains("查詢待辦") || normalized.contains("列出待辦") || normalized.contains("看待辦") {
        let entries = match tracker.read_last_n(100) {
            Ok(v) => v,
            Err(e) => return Some(format!("執行結果：讀取待辦失敗，原因：{e}")),
        };

        let actionable: Vec<&TaskEntry> = entries
            .iter()
            .filter(|e| matches!(e.status.as_deref(), Some("PENDING") | Some("FOLLOWING") | Some("FOLLOWUP_NEEDED")))
            .collect();

        if actionable.is_empty() {
            return Some("執行結果：目前沒有待辦任務。".to_string());
        }

        let preview = actionable
            .iter()
            .take(3)
            .map(|e| {
                let status = e.status.as_deref().unwrap_or("?");
                let reason = e.reason.as_deref().unwrap_or("(無描述)");
                format!("- {status}: {reason}")
            })
            .collect::<Vec<_>>()
            .join("\n");

        return Some(format!(
            "執行結果：目前共有 {} 筆待辦。\n{}",
            actionable.len(),
            preview
        ));
    }

    // 3) Complete the latest pending task.
    if normalized.contains("完成最新待辦") || normalized.contains("完成待辦") {
        let entries = match tracker.read_last_n(200) {
            Ok(v) => v,
            Err(e) => return Some(format!("執行結果：讀取待辦失敗，原因：{e}")),
        };

        let target = entries
            .iter()
            .rev()
            .find(|e| matches!(e.status.as_deref(), Some("PENDING") | Some("FOLLOWING") | Some("FOLLOWUP_NEEDED")));

        if let Some(item) = target {
            let mut updates = HashMap::new();
            updates.insert(item.timestamp.clone(), "DONE".to_string());
            return match tracker.update_statuses(&updates) {
                Ok(_) => Some("執行結果：已將最新待辦標記為 DONE。".to_string()),
                Err(e) => Some(format!("執行結果：更新待辦失敗，原因：{e}")),
            };
        }

        return Some("執行結果：沒有可完成的待辦。".to_string());
    }

    None
}

fn contains_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        (ch >= '\u{4E00}' && ch <= '\u{9FFF}')
            || (ch >= '\u{3400}' && ch <= '\u{4DBF}')
            || (ch >= '\u{F900}' && ch <= '\u{FAFF}')
    })
}

fn chinese_fallback_reply(
    persona_name: &str,
    voice: &str,
    compliance: &str,
    estimated_profit: f64,
    execution_result: Option<&str>,
) -> String {
    let mut base = format!(
        "收到你的訊息。我是{}，會用{}的語氣回覆你。{}（估計收益 {:.2} USD）",
        persona_name, voice, compliance, estimated_profit
    );

    if let Some(result) = execution_result {
        base.push_str(&format!("\n執行結果：{}", result));
    }

    base
}

fn build_ai_reply_prompt(
    persona: Option<&Persona>,
    user_text: &str,
    estimated_profit: f64,
    execution_result: Option<&str>,
) -> String {
    let persona_name = persona.map(|p| p.name()).unwrap_or("Sirin");
    let (voice, compliance) = persona
        .map(|p| {
            (
                p.response_style.voice.as_str(),
                p.response_style.compliance_line.as_str(),
            )
        })
        .unwrap_or(("natural, polite, professional", "Follow the user's request step by step."));

    let execution_block = execution_result
        .map(|v| format!("Execution result from internal action layer: {v}"))
        .unwrap_or_else(|| "Execution result from internal action layer: no direct action executed.".to_string());

    format!(
        "You are {persona_name}.\n\
Use this persona style: {voice}.\n\
Core rule: {compliance}\n\
Task: Reply to the latest user message naturally and helpfully.\n\
Constraints:\n\
- Keep response concise (1-3 sentences).\n\
- Be polite and human-like.\n\
- Reply in the same language as the user's message.\n\
- If an internal action already ran, include a short result summary.\n\
\n\
User message: {user_text}\n\
Estimated ROI score: {estimated_profit:.2}\n\
{execution_block}\n\
\n\
Return only the final reply text."
    )
}

async fn call_ollama_reply(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: String,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{base_url}/api/generate");
    let body = OllamaRequest {
        model,
        prompt,
        stream: false,
    };
    let resp: OllamaResponse = client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.response.trim().to_string())
}

async fn call_openai_reply(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    prompt: String,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/chat/completions");

    let body = OpenAiRequest {
        model,
        messages: vec![OpenAiMessage {
            role: "user",
            content: prompt,
        }],
        stream: false,
    };

    let mut req = client.post(&url).json(&body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }

    let resp: OpenAiResponse = req.send().await?.error_for_status()?.json().await?;
    let content = resp
        .choices
        .first()
        .map(|c| c.message.content.trim().to_string())
        .unwrap_or_default();

    Ok(content)
}

async fn generate_ai_reply(
    client: &reqwest::Client,
    llm: &ReplyLlmConfig,
    persona: Option<&Persona>,
    user_text: &str,
    estimated_profit: f64,
    execution_result: Option<&str>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = build_ai_reply_prompt(persona, user_text, estimated_profit, execution_result);
    match llm.backend {
        ReplyLlmBackend::Ollama => call_ollama_reply(client, &llm.base_url, &llm.model, prompt).await,
        ReplyLlmBackend::LmStudio => {
            call_openai_reply(
                client,
                &llm.base_url,
                &llm.model,
                llm.api_key.as_deref(),
                prompt,
            )
            .await
        }
    }
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
    let llm = ReplyLlmConfig::from_env();
    let llm_client = reqwest::Client::new();

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
                // Only handle fresh updates after startup to avoid bulk auto-replies.
                catch_up: false,
                ..Default::default()
            },
        )
        .await;

    eprintln!("[telegram] Connected to Telegram");
    let backend_name = match llm.backend {
        ReplyLlmBackend::Ollama => "ollama",
        ReplyLlmBackend::LmStudio => "lmstudio",
    };
    eprintln!(
        "[telegram] AI reply backend={} model='{}'",
        backend_name, llm.model
    );
    if cfg.debug_updates {
        eprintln!(
            "[telegram] debug_updates=on, reply_private={}, reply_groups={}, auto_reply_enabled={}",
            cfg.reply_private, cfg.reply_groups, cfg.auto_reply_enabled
        );
    }
    let listener_started_at = Utc::now();

    // Send startup notification to self
    if let Some(ref msg) = cfg.startup_msg {
        eprintln!("[telegram] TG_STARTUP_MSG is enabled");
        match client.get_me().await {
            Ok(me) => {
                eprintln!(
                    "[telegram] Authorized as id={}, username={:?}, name='{} {}'",
                    me.id().bare_id(),
                    me.username(),
                    me.first_name().unwrap_or(""),
                    me.last_name().unwrap_or("")
                );
                let target_peer_ref = if let Some(ref username) = cfg.startup_target {
                    match client.resolve_username(username).await {
                        Ok(Some(peer)) => peer.to_ref().await,
                        Ok(None) => {
                            eprintln!("[telegram] TG_STARTUP_TARGET '@{username}' not found");
                            None
                        }
                        Err(e) => {
                            eprintln!("[telegram] Failed to resolve TG_STARTUP_TARGET '@{username}': {e}");
                            None
                        }
                    }
                } else {
                    me.to_ref().await
                };

                if let Some(peer_ref) = target_peer_ref {
                    let text = msg
                        .replace("{time}", &Utc::now().format("%Y-%m-%d %H:%M UTC").to_string());
                    if let Err(e) = client.send_message(peer_ref, text.as_str()).await {
                        eprintln!("[telegram] Failed to send startup message: {e}");
                    } else {
                        if let Some(ref username) = cfg.startup_target {
                            eprintln!("[telegram] Startup message sent to @{username}");
                        } else {
                            eprintln!("[telegram] Startup message sent to self");
                        }
                    }
                } else {
                    if cfg.startup_target.is_some() {
                        eprintln!("[telegram] Could not resolve startup target peer_ref, startup message skipped");
                    } else {
                        eprintln!("[telegram] Could not resolve self peer_ref, startup message skipped");
                    }
                }
            }
            Err(e) => eprintln!("[telegram] get_me failed: {e}"),
        }
    } else {
        eprintln!("[telegram] TG_STARTUP_MSG is not set, startup message disabled");
    }

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

        if cfg.debug_updates {
            eprintln!(
                "[telegram] incoming: sender={:?}, peer={:?}, outgoing={}, date={}, text='{}'",
                message.sender_id(),
                message.peer_id(),
                message.outgoing(),
                message.date(),
                message.text().chars().take(80).collect::<String>()
            );
        }

        if message.outgoing() {
            if cfg.debug_updates {
                eprintln!("[telegram] skip: outgoing message");
            }
            continue;
        }

        // Guard against self-chat feedback loops (e.g. startup message in Saved Messages).
        if message.sender_id() == Some(PeerId::self_user()) {
            if cfg.debug_updates {
                eprintln!("[telegram] skip: sender is self_user");
            }
            continue;
        }

        let is_private = matches!(message.peer_id().kind(), PeerKind::User | PeerKind::UserSelf);
        if is_private && !cfg.reply_private {
            if cfg.debug_updates {
                eprintln!("[telegram] skip: private replies disabled");
            }
            continue;
        }

        if !is_private && !cfg.reply_groups {
            if cfg.debug_updates {
                eprintln!("[telegram] skip: group replies disabled");
            }
            continue;
        }

        // TG_GROUP_IDS only applies to group/channel chats.
        if !is_private && !cfg.group_ids.is_empty() {
            let peer_id = message.peer_id().bare_id();
            if !cfg.group_ids.contains(&peer_id) {
                if cfg.debug_updates {
                    eprintln!("[telegram] skip: group id {} not in TG_GROUP_IDS", peer_id);
                }
                continue;
            }
        }

        // Ignore messages that predate current listener run.
        if message.date() < listener_started_at {
            if cfg.debug_updates {
                eprintln!(
                    "[telegram] skip: message older than listener start ({} < {})",
                    message.date(),
                    listener_started_at
                );
            }
            continue;
        }

        let text = message.text().to_owned();
        if text.is_empty() {
            if cfg.debug_updates {
                eprintln!("[telegram] skip: empty text message");
            }
            continue;
        }

        let persona = Persona::load().ok();
        let persona_name = persona.as_ref().map(|p| p.name()).unwrap_or("Sirin");

        let estimated_profit = estimate_profit(&text);
        let triggered = persona
            .as_ref()
            .map(|p| p.should_trigger_remote_ai(estimated_profit))
            .unwrap_or(false);

        if cfg.auto_reply_enabled {
            let (voice, ack_prefix, compliance) = persona
                .as_ref()
                .map(|p| {
                    (
                        p.response_style.voice.as_str(),
                        p.response_style.ack_prefix.as_str(),
                        p.response_style.compliance_line.as_str(),
                    )
                })
                .unwrap_or(("自然、禮貌、專業", "已收到你的訊息。", "我會按照你的要求處理。"));

            let execution_result = execute_user_request(&text, &tracker, persona_name);
            let fallback_reply = cfg
                .auto_reply_text
                .replace("{persona}", persona_name)
                .replace("{profit}", &format!("{estimated_profit:.2}"))
                .replace("{voice}", voice)
                .replace("{ack_prefix}", ack_prefix)
                .replace("{compliance}", compliance);

            let ai_reply = match generate_ai_reply(
                &llm_client,
                &llm,
                persona.as_ref(),
                &text,
                estimated_profit,
                execution_result.as_deref(),
            )
            .await
            {
                Ok(v) if !v.trim().is_empty() => v,
                Ok(_) => fallback_reply.clone(),
                Err(e) => {
                    eprintln!("[telegram] AI reply generation failed, fallback to template: {e}");
                    fallback_reply.clone()
                }
            };

            let final_reply = if contains_cjk(&text) && !contains_cjk(&ai_reply) {
                eprintln!("[telegram] AI reply language mismatch: forcing Chinese response");
                chinese_fallback_reply(
                    persona_name,
                    voice,
                    compliance,
                    estimated_profit,
                    execution_result.as_deref(),
                )
            } else {
                ai_reply
            };

            if let Err(e) = message.reply(final_reply).await {
                eprintln!("[telegram] Failed to auto-reply: {e}");
            } else {
                eprintln!("[telegram] Auto-reply sent (AI path)");
            }
        } else if cfg.debug_updates {
            eprintln!("[telegram] skip: auto-reply disabled by TG_AUTO_REPLY");
        }

        let entry = TaskEntry::ai_decision(persona_name, estimated_profit, triggered);
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
                "[telegram] ROI trigger fired (profit={estimated_profit:.2}, persona={persona_name})",
            );
        }
    }
}
