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

use crate::memory::{append_context, load_recent_context};
use crate::persona::{Persona, TaskEntry, TaskTracker};
use crate::researcher;
use crate::skills::ddg_search;
use crate::telegram_auth::TelegramAuthState;

const OLLAMA_BASE_URL: &str = "http://localhost:11434";
const LM_STUDIO_BASE_URL: &str = "http://localhost:1234/v1";
const DEFAULT_MODEL: &str = "llama3.2";

fn message_preview(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

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
            .unwrap_or_else(|| "{ack_prefix} 我會先幫你處理這件事。".to_string());

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

/// Code + 2FA timeout: how long (seconds) we wait for the user to enter credentials via UI.
const AUTH_INPUT_TIMEOUT_SECS: u64 = 300;

/// Whether Telegram sign-in is required at startup.
///
/// Default is optional (`false`) so the desktop app can run without waiting
/// for Telegram credentials. Set `TG_REQUIRE_LOGIN=1` to enforce login.
fn require_login() -> bool {
    env::var("TG_REQUIRE_LOGIN")
        .ok()
        .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Non-blocking sign-in helper.
///
/// Instead of reading from stdin (which hangs in the Tauri GUI process), this
/// function tells the frontend that credentials are needed via `TelegramAuthState`,
/// then awaits a bounded-time channel receive.  If the user does not respond
/// within `AUTH_INPUT_TIMEOUT_SECS` the function returns an error and the
/// listener is retired; the retry loop in `run_listener_with_retry` will
/// attempt again shortly.
async fn ensure_user_authorized(
    client: &Client,
    cfg: &TelegramConfig,
    auth: &TelegramAuthState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if client.is_authorized().await? {
        return Ok(());
    }

    if !require_login() {
        auth.set_disconnected("telegram login optional; set TG_REQUIRE_LOGIN=1 to enable auth flow");
        eprintln!(
            "[telegram] Session not authorized; login is optional, skipping sign-in flow"
        );
        return Ok(());
    }

    eprintln!("[telegram] Session not authorized, starting user sign-in flow...");

    let phone = cfg.phone.clone().ok_or_else(|| {
        "TG_PHONE is not set; cannot start non-interactive sign-in (set TG_PHONE in .env)"
    })?;

    let login_token = client.request_login_code(&phone, &cfg.api_hash).await?;
    eprintln!("[telegram] Login code requested for {phone}; waiting for UI input (timeout {}s)", AUTH_INPUT_TIMEOUT_SECS);

    let code = auth
        .request_code(AUTH_INPUT_TIMEOUT_SECS)
        .await
        .ok_or("Timed out waiting for Telegram login code from UI")?;

    match client.sign_in(&login_token, &code).await {
        Ok(_) => {
            eprintln!("[telegram] User sign-in succeeded");
            Ok(())
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            let hint = password_token.hint().unwrap_or("none").to_string();
            eprintln!("[telegram] 2FA required (hint: {hint}); waiting for UI input");
            let password = auth
                .request_password(&hint, AUTH_INPUT_TIMEOUT_SECS)
                .await
                .ok_or("Timed out waiting for Telegram 2FA password from UI")?;
            client.check_password(password_token, password.trim()).await?;
            eprintln!("[telegram] User sign-in with 2FA succeeded");
            Ok(())
        }
        Err(err) => Err(format!("Telegram sign-in failed: {err}").into()),
    }
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
            correlation_id: None,
            message_preview: Some(message_preview(normalized, 140)),
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

fn is_mixed_language_reply(text: &str) -> bool {
    let mut cjk_count = 0usize;
    let mut latin_count = 0usize;

    for ch in text.chars() {
        if (ch >= '\u{4E00}' && ch <= '\u{9FFF}')
            || (ch >= '\u{3400}' && ch <= '\u{4DBF}')
            || (ch >= '\u{F900}' && ch <= '\u{FAFF}')
        {
            cjk_count += 1;
        } else if ch.is_ascii_alphabetic() {
            latin_count += 1;
        }
    }

    if cjk_count == 0 || latin_count == 0 {
        return false;
    }

    let total = cjk_count + latin_count;
    let latin_ratio = latin_count as f32 / total as f32;

    // Treat as mixed when there are enough Latin letters to impact readability.
    latin_count >= 8 && latin_ratio > 0.35
}

fn is_direct_answer_request(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    normalized.contains("直接跟我說")
        || normalized.contains("直接說")
        || normalized.contains("直接講")
        || normalized.contains("不要貼連結")
        || normalized.contains("別貼連結")
        || normalized.contains("不用連結")
        || normalized.contains("just tell me")
        || normalized.contains("no links")
}

fn chinese_fallback_reply(user_text: &str, execution_result: Option<&str>) -> String {
    let mut base = if user_text.trim().len() <= 12 {
        "收到，我在這裡。你想先從哪一點開始？".to_string()
    } else {
        "收到，我理解你的需求了；我先幫你整理重點，接著給你可執行的下一步。".to_string()
    };

    if let Some(result) = execution_result {
        base.push_str(&format!("\n{result}"));
    }

    base
}

fn should_search(text: &str) -> bool {
    let lower = text.to_lowercase();
    text.contains('?')
        || text.contains('？')
        || lower.contains("什麼")
        || lower.contains("如何")
        || lower.contains("為什麼")
        || lower.contains("怎麼")
        || lower.contains("哪裡")
        || lower.contains("what")
        || lower.contains("how")
        || lower.contains("why")
        || lower.contains("when")
        || lower.contains("where")
        || lower.contains("who")
}

/// Detect if a message is a research request.
///
/// Returns `Some((topic, url))` when the message starts with a research keyword.
/// The URL is extracted from the message if present.
fn detect_research_intent(text: &str) -> Option<(String, Option<String>)> {
    let normalized = text.trim();
    let lower = normalized.to_lowercase();

    let is_research = lower.starts_with("調研")
        || lower.starts_with("研究")
        || lower.starts_with("幫我研究")
        || lower.starts_with("幫我調研")
        || lower.starts_with("幫我查一下")
        || lower.starts_with("幫我查")
        || lower.starts_with("深入研究")
        || lower.starts_with("背景調研");

    if !is_research {
        return None;
    }

    // Extract URL using simple pattern matching.
    let url = normalized
        .split_whitespace()
        .find(|token| token.starts_with("http://") || token.starts_with("https://"))
        .map(|s| s.to_string());

    // The topic is the full message text, trimmed of the keyword.
    let topic = normalized
        .trim_start_matches("幫我調研")
        .trim_start_matches("幫我研究")
        .trim_start_matches("幫我查一下")
        .trim_start_matches("幫我查")
        .trim_start_matches("深入研究")
        .trim_start_matches("背景調研")
        .trim_start_matches("調研")
        .trim_start_matches("研究")
        .trim()
        .to_string();

    Some((if topic.is_empty() { normalized.to_string() } else { topic }, url))
}

fn build_ai_reply_prompt(
    persona: Option<&Persona>,
    user_text: &str,
    execution_result: Option<&str>,
    search_context: Option<&str>,
    context_block: Option<&str>,
    direct_answer_request: bool,
    force_traditional_chinese: bool,
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
        .map(|v| format!("\nExecution result from internal action layer: {v}"))
        .unwrap_or_default();

    let search_block = search_context
        .map(|v| format!("\nWeb search results (use as reference, do not quote verbatim):\n{v}"))
        .unwrap_or_default();

    let history_block = context_block
        .map(|v| format!("\nRecent conversation history:\n{v}"))
        .unwrap_or_default();

    let language_override = if force_traditional_chinese {
        "- Reply in Traditional Chinese only.\n"
    } else {
        ""
    };

    let direct_mode_constraints = if direct_answer_request {
        "- The user asked for a direct answer: provide concrete steps immediately.\n\
- Do not include external links unless the user explicitly asks for links.\n\
"
    } else {
        ""
    };

    format!(
        "You are {persona_name}.\n\
Use this persona style: {voice}.\n\
Core rule: {compliance}\n\
Task: Reply to the latest user message naturally and helpfully.\n\
Constraints:\n\
- Keep response concise (1-3 sentences).\n\
- Be polite and human-like.\n\
- Reply in the same language as the user's message.\n\
- Continue from the recent conversation context instead of restarting the topic.\n\
- Do not self-introduce unless the user asks who you are.\n\
- Avoid sounding like a system prompt or policy statement.\n\
{language_override}
{direct_mode_constraints}
- If an internal action already ran, include a short result summary.\n\
\n\
User message: {user_text}\n\
{execution_block}{search_block}{history_block}\n\
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
    execution_result: Option<&str>,
    search_context: Option<&str>,
    context_block: Option<&str>,
    direct_answer_request: bool,
    force_traditional_chinese: bool,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = build_ai_reply_prompt(
        persona,
        user_text,
        execution_result,
        search_context,
        context_block,
        direct_answer_request,
        force_traditional_chinese,
    );
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

/// Inner listener — connects once, runs until an unrecoverable error.
async fn run_listener_once(
    tracker: &TaskTracker,
    auth: &TelegramAuthState,
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

    if let Err(err) = ensure_user_authorized(&client, &cfg, auth).await {
        eprintln!("[telegram] {err}");
        handle.quit();
        let _ = pool_task.await;
        return Ok(());
    }

    if !client.is_authorized().await.unwrap_or(false) {
        eprintln!("[telegram] Session remains unauthorized; skipping listener run");
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
        let mut should_record_ai_decision = false;

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

            // Check for research intent before normal command dispatch.
            let research_execution: Option<String> = if let Some((topic, url)) = detect_research_intent(&text) {
                let topic_clone = topic.clone();
                let url_clone = url.clone();
                tokio::spawn(async move {
                    let task = researcher::run_research(topic_clone, url_clone).await;
                    eprintln!("[researcher] Background task '{}' completed with status={:?}", task.id, task.status);
                });
                let url_hint = url.map(|u| format!(" ({})", u)).unwrap_or_default();
                Some(format!(
                    "執行結果：已啟動背景調研任務「{}{}」，完成後結果將記錄在任務板。",
                    topic, url_hint
                ))
            } else {
                None
            };

            let execution_result = research_execution
                .or_else(|| execute_user_request(&text, &tracker, persona_name));
            should_record_ai_decision = execution_result.is_some();
            let direct_answer_request = is_direct_answer_request(&text);
            let fallback_reply = cfg
                .auto_reply_text
                .replace("{persona}", persona_name)
                .replace("{voice}", voice)
                .replace("{ack_prefix}", ack_prefix)
                .replace("{compliance}", compliance);

            // Load recent conversation context.
            let context_block: Option<String> = match load_recent_context(5) {
                Ok(entries) if !entries.is_empty() => {
                    let formatted = entries
                        .iter()
                        .map(|e| format!("User: {}\nAssistant: {}", e.user_msg, e.assistant_reply))
                        .collect::<Vec<_>>()
                        .join("\n---\n");
                    Some(formatted)
                }
                Ok(_) => None,
                Err(e) => {
                    eprintln!("[telegram] Failed to load context: {e}");
                    None
                }
            };

            // Perform web search when the message looks like a question.
            let search_context: Option<String> = if !direct_answer_request && should_search(&text) {
                let query = text.chars().take(100).collect::<String>();
                eprintln!("[telegram] Searching web for: {query}");
                match ddg_search(&query).await {
                    Ok(results) if !results.is_empty() => {
                        let formatted = results
                            .iter()
                            .take(3)
                            .map(|r| format!("- {}: {} ({})", r.title, r.snippet, r.url))
                            .collect::<Vec<_>>()
                            .join("\n");
                        eprintln!("[telegram] Web search returned {} result(s)", results.len().min(3));
                        Some(formatted)
                    }
                    Ok(_) => {
                        eprintln!("[telegram] Web search returned no results");
                        None
                    }
                    Err(e) => {
                        eprintln!("[telegram] Web search failed: {e}");
                        None
                    }
                }
            } else {
                None
            };

            let ai_reply = match generate_ai_reply(
                &llm_client,
                &llm,
                persona.as_ref(),
                &text,
                execution_result.as_deref(),
                search_context.as_deref(),
                context_block.as_deref(),
                direct_answer_request,
                false,
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

            let final_reply = if contains_cjk(&text)
                && (!contains_cjk(&ai_reply) || is_mixed_language_reply(&ai_reply))
            {
                eprintln!("[telegram] AI reply language mismatch/mixed output: retrying with forced Traditional Chinese");
                match generate_ai_reply(
                    &llm_client,
                    &llm,
                    persona.as_ref(),
                    &text,
                    execution_result.as_deref(),
                    search_context.as_deref(),
                    context_block.as_deref(),
                    direct_answer_request,
                    true,
                )
                .await
                {
                    Ok(v) if !v.trim().is_empty() && contains_cjk(&v) => v,
                    Ok(_) | Err(_) => chinese_fallback_reply(&text, execution_result.as_deref()),
                }
            } else {
                ai_reply
            };

            let reply_for_context = final_reply.clone();

            if is_private {
                if let Some(peer_ref) = message.peer_ref().await {
                    match client.send_message(peer_ref, final_reply.as_str()).await {
                        Ok(_) => eprintln!("[telegram] Auto-reply sent (AI path, private direct message)"),
                        Err(e) => {
                            eprintln!("[telegram] Private direct send failed, fallback to reply: {e}");
                            if let Err(reply_err) = message.reply(final_reply).await {
                                eprintln!("[telegram] Failed to auto-reply: {reply_err}");
                            } else {
                                eprintln!("[telegram] Auto-reply sent (AI path, private reply fallback)");
                            }
                        }
                    }
                } else if let Err(reply_err) = message.reply(final_reply).await {
                    eprintln!("[telegram] Private peer_ref missing and reply failed: {reply_err}");
                } else {
                    eprintln!("[telegram] Private peer_ref missing, sent via reply fallback");
                }
            } else if let Err(e) = message.reply(final_reply).await {
                eprintln!("[telegram] Failed to auto-reply: {e}");
            } else {
                eprintln!("[telegram] Auto-reply sent (AI path, group reply)");
            }

            if let Err(e) = append_context(&text, &reply_for_context) {
                eprintln!("[telegram] Failed to save context: {e}");
            }
        } else if cfg.debug_updates {
            eprintln!("[telegram] skip: auto-reply disabled by TG_AUTO_REPLY");
        }

        if should_record_ai_decision {
            let entry = TaskEntry::ai_decision(
                persona_name,
                Some(message_preview(&text, 140)),
            );
            if let Err(e) = tracker.record(&entry) {
                eprintln!("[telegram] Failed to record task entry: {e}");
            }
        }
    }
    Ok(())
}

/// Public entry-point: wraps `run_listener_once` with automatic retry so that
/// a missing session, a failed sign-in, or a transient network error never
/// blocks the application.
///
/// # Retry policy
/// - On a clean exit (env vars missing / auth unavailable): wait 60 s before
///   retrying so the user can update `.env` or enter credentials via the UI.
/// - On a connection error: wait 30 s.
/// - Maximum back-off is capped at 5 minutes.
pub async fn run_listener(tracker: TaskTracker, auth: TelegramAuthState) {
    let mut backoff_secs: u64 = 30;
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        eprintln!("[telegram] Starting listener attempt #{attempt}");
        auth.set_disconnected(format!("attempt #{attempt}"));

        match run_listener_once(&tracker, &auth).await {
            Ok(()) => {
                // run_listener_once returned Ok when auth was unavailable or
                // env vars were absent — wait before retrying.
                eprintln!("[telegram] Listener exited cleanly; retrying in {backoff_secs}s");
            }
            Err(e) => {
                eprintln!("[telegram] Listener error: {e}; retrying in {backoff_secs}s");
                auth.set_error(e.to_string());
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;

        // Exponential back-off capped at 5 minutes.
        backoff_secs = (backoff_secs * 2).min(300);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn research_intent_url_extracted() {
        let (topic, url) = detect_research_intent("調研 https://agoramarket.purrtechllc.com/").unwrap();
        assert!(url.as_deref() == Some("https://agoramarket.purrtechllc.com/"), "url={url:?}");
        assert!(topic.contains("agoramarket"), "topic={topic}");
    }

    #[test]
    fn research_intent_topic_only() {
        let (topic, url) = detect_research_intent("幫我研究 Rust async runtime 的工作原理").unwrap();
        assert!(url.is_none());
        assert!(topic.contains("Rust"), "topic={topic}");
    }

    #[test]
    fn research_intent_not_triggered() {
        assert!(detect_research_intent("你好嗎").is_none());
        assert!(detect_research_intent("what is the weather?").is_none());
    }

    #[test]
    fn research_intent_various_prefixes() {
        assert!(detect_research_intent("幫我調研 某主題").is_some());
        assert!(detect_research_intent("背景調研 https://example.com").is_some());
        assert!(detect_research_intent("深入研究 某主題").is_some());
    }

    #[test]
    fn should_search_triggers_on_question_words() {
        assert!(should_search("什麼是 Rust？"));
        assert!(should_search("how does async work?"));
        assert!(should_search("why is the sky blue"));
        assert!(!should_search("你好"));
    }

    #[test]
    fn direct_answer_request_detected() {
        assert!(is_direct_answer_request("你直接跟我說"));
        assert!(is_direct_answer_request("直接講重點，不要貼連結"));
        assert!(is_direct_answer_request("just tell me the steps, no links"));
        assert!(!is_direct_answer_request("請幫我查一下這個主題"));
    }

    #[test]
    fn mixed_language_reply_detection() {
        assert!(is_mixed_language_reply("Sorry吧，我直接跟你說了，nothing 的發生。"));
        assert!(!is_mixed_language_reply("我直接跟你說：先準備文件，再到網站提交 90 天報到。"));
    }
}
