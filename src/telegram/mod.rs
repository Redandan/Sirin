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

pub(crate) mod commands;
mod config;
mod handler;
pub(crate) mod language;
pub(crate) mod llm;
mod reply;

use std::sync::Arc;

use chrono::Utc;
use grammers_client::client::UpdatesConfiguration;
use grammers_client::SignInError;
use grammers_client::{Client, SenderPool};
use grammers_client::update::Update;
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerId, PeerKind};

use crate::memory::ensure_codebase_index;
use crate::sirin_log;
use crate::persona::{Persona, TaskTracker};
use crate::researcher;
use crate::telegram_auth::TelegramAuthState;

use config::{require_login, session_path, TelegramConfig, AUTH_INPUT_TIMEOUT_SECS};

// ── Auth helper ───────────────────────────────────────────────────────────────

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
        sirin_log!(
            "[telegram] Session not authorized; login is optional, skipping sign-in flow"
        );
        return Ok(());
    }

    sirin_log!("[telegram] Session not authorized, starting user sign-in flow...");

    let phone = cfg.phone.clone().ok_or_else(|| {
        "TG_PHONE is not set; cannot start non-interactive sign-in (set TG_PHONE in .env)"
    })?;

    let login_token = client.request_login_code(&phone, &cfg.api_hash).await?;
    sirin_log!("[telegram] Login code requested for {phone}; waiting for UI input (timeout {}s)", AUTH_INPUT_TIMEOUT_SECS);

    let code = auth
        .request_code(AUTH_INPUT_TIMEOUT_SECS)
        .await
        .ok_or("Timed out waiting for Telegram login code from UI")?;

    match client.sign_in(&login_token, &code).await {
        Ok(_) => {
            sirin_log!("[telegram] User sign-in succeeded");
            Ok(())
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            let hint = password_token.hint().unwrap_or("none").to_string();
            sirin_log!("[telegram] 2FA required (hint: {hint}); waiting for UI input");
            let password = auth
                .request_password(&hint, AUTH_INPUT_TIMEOUT_SECS)
                .await
                .ok_or("Timed out waiting for Telegram 2FA password from UI")?;
            client.check_password(password_token, password.trim()).await?;
            sirin_log!("[telegram] User sign-in with 2FA succeeded");
            Ok(())
        }
        Err(err) => Err(format!("Telegram sign-in failed: {err}").into()),
    }
}

// ── Listener ──────────────────────────────────────────────────────────────────

/// Inner listener — connects once, runs until an unrecoverable error.
async fn run_listener_once(
    tracker: &TaskTracker,
    auth: &TelegramAuthState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cfg = TelegramConfig::from_env()?;
    let llm = crate::llm::shared_llm();
    let http = crate::llm::shared_http();

    if let Err(e) = ensure_codebase_index() {
        sirin_log!("[telegram] Codebase index refresh skipped: {e}");
    }

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
        sirin_log!("[telegram] {err}");
        handle.quit();
        let _ = pool_task.await;
        return Ok(());
    }

    if !client.is_authorized().await.unwrap_or(false) {
        sirin_log!("[telegram] Session remains unauthorized; skipping listener run");
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

    sirin_log!("[telegram] Connected to Telegram");
    auth.set_connected();
    let backend_name = llm.backend_name();
    sirin_log!(
        "[telegram] AI reply backend={} model='{}'",
        backend_name, llm.model
    );
    if cfg.debug_updates {
        sirin_log!(
            "[telegram] debug_updates=on, reply_private={}, reply_groups={}, auto_reply_enabled={}",
            cfg.reply_private, cfg.reply_groups, cfg.auto_reply_enabled
        );
    }
    let listener_started_at = Utc::now();

    reply::send_startup_message(&client, &cfg).await;

    loop {
        let update = match updates.next().await {
            Ok(u) => u,
            Err(e) => {
                sirin_log!("[telegram] Error receiving update: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let Update::NewMessage(message) = update else {
            continue;
        };

        if cfg.debug_updates {
            sirin_log!(
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
                sirin_log!("[telegram] skip: outgoing message");
            }
            continue;
        }

        // Guard against self-chat feedback loops (e.g. startup message in Saved Messages).
        if message.sender_id() == Some(PeerId::self_user()) {
            if cfg.debug_updates {
                sirin_log!("[telegram] skip: sender is self_user");
            }
            continue;
        }

        let is_private = matches!(message.peer_id().kind(), PeerKind::User | PeerKind::UserSelf);
        if is_private && !cfg.reply_private {
            if cfg.debug_updates {
                sirin_log!("[telegram] skip: private replies disabled");
            }
            continue;
        }

        if !is_private && !cfg.reply_groups {
            if cfg.debug_updates {
                sirin_log!("[telegram] skip: group replies disabled");
            }
            continue;
        }

        // TG_GROUP_IDS only applies to group/channel chats.
        if !is_private && !cfg.group_ids.is_empty() {
            let peer_id = message.peer_id().bare_id();
            if !cfg.group_ids.contains(&peer_id) {
                if cfg.debug_updates {
                    sirin_log!("[telegram] skip: group id {} not in TG_GROUP_IDS", peer_id);
                }
                continue;
            }
        }

        // Ignore messages that predate current listener run.
        if message.date() < listener_started_at {
            if cfg.debug_updates {
                sirin_log!(
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
                sirin_log!("[telegram] skip: empty text message");
            }
            continue;
        }

        let persona = Persona::load().ok();
        let persona_name = persona.as_ref().map(|p| p.name()).unwrap_or("Sirin");
        let peer_bare_id = Some(message.peer_id().bare_id());
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

            let reply_plan = handler::prepare_reply_plan(
                &text,
                persona_name,
                voice,
                ack_prefix,
                compliance,
                tracker,
                &cfg,
                |topic, url| {
                    let adk_tracker = tracker.clone();
                    let notify_handle = handle.clone();
                    let notify_peer_fut = message.peer_ref();
                    async move {
                        let notify_peer = notify_peer_fut.await;
                        tokio::spawn(async move {
                            let task = crate::agents::research_agent::run_research_via_adk_with_tracker(
                                topic,
                                url,
                                Some(adk_tracker),
                            )
                            .await;
                            sirin_log!("[researcher] Background task '{}' completed with status={:?}", task.id, task.status);
                            if task.status == researcher::ResearchStatus::Done {
                                if let (Some(ref report), Some(peer)) = (&task.final_report, notify_peer) {
                                    let summary: String = report.chars().take(500).collect();
                                    let msg = format!("✅ 調研完成：{}\n\n{}", task.topic, summary);
                                    let notify_client = Client::new(notify_handle);
                                    if let Err(e) = notify_client.send_message(peer, msg.as_str()).await {
                                        sirin_log!("[researcher] Failed to notify user of completion: {e}");
                                    } else {
                                        sirin_log!("[researcher] Research completion notified to user");
                                    }
                                }
                            }
                        });
                    }
                },
            )
            .await;
            should_record_ai_decision = reply_plan.should_record_ai_decision;

            // ── Choose streaming vs. ADK path ─────────────────────────────────
            // Streaming path: simple direct queries with no research action.
            // ADK path: research intents or queries needing ReAct tool use.
            let use_streaming = reply_plan.execution_result.is_none()
                && !crate::telegram::commands::detect_research_intent(&text).is_some();

            let final_reply = if use_streaming {
                // Build prompt and stream tokens back to Telegram.
                let persona = Persona::load().ok();
                let recent_ctx = crate::memory::load_recent_context(5, peer_bare_id)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|entries| {
                        entries.iter()
                            .map(|e| format!("User: {}\nAssistant: {}", e.user_msg, e.assistant_reply))
                            .collect::<Vec<_>>()
                            .join("\n---\n")
                    });
                let search_ctx = if crate::telegram::commands::should_search(&text) {
                    let query = crate::telegram::commands::extract_search_query(http.as_ref(), llm.as_ref(), &text).await;
                    crate::skills::ddg_search(&query).await.ok()
                        .filter(|r| !r.is_empty())
                        .map(|r| r.iter().take(3)
                            .map(|sr| format!("- {}: {} ({})", sr.title, sr.snippet, sr.url))
                            .collect::<Vec<_>>().join("\n"))
                } else { None };

                let prompt = crate::telegram::llm::build_ai_reply_prompt(
                    persona.as_ref(),
                    &text,
                    None,
                    search_ctx.as_deref(),
                    recent_ctx.as_deref(),
                    None,
                    None,
                    false,
                    false,
                );

                reply::send_streaming_reply(&client, &message, is_private, prompt, llm.as_ref(), http.as_ref()).await
            } else {
                // ADK path: research intents and complex queries.
                let reply = crate::agents::chat_agent::run_chat_via_adk_with_tracker(
                    crate::agents::chat_agent::ChatRequest {
                        user_text: text.clone(),
                        execution_result: reply_plan.execution_result.clone(),
                        context_block: None,
                        fallback_reply: Some(reply_plan.fallback_reply),
                        peer_id: peer_bare_id,
                    },
                    Some(tracker.clone()),
                )
                .await;
                reply::send_final_reply(&client, &message, is_private, reply.as_str()).await;
                reply
            };

            reply::persist_reply_context(&text, &final_reply, peer_bare_id);
        } else if cfg.debug_updates {
            sirin_log!("[telegram] skip: auto-reply disabled by TG_AUTO_REPLY");
        }

        reply::record_ai_decision_if_needed(
            tracker,
            persona_name,
            &text,
            should_record_ai_decision,
        );
    }
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
        sirin_log!("[telegram] Starting listener attempt #{attempt}");
        auth.set_disconnected(format!("attempt #{attempt}"));

        match run_listener_once(&tracker, &auth).await {
            Ok(()) => {
                // run_listener_once returned Ok when auth was unavailable or
                // env vars were absent — wait before retrying.
                sirin_log!("[telegram] Listener exited cleanly; retrying in {backoff_secs}s");
            }
            Err(e) => {
                sirin_log!("[telegram] Listener error: {e}; retrying in {backoff_secs}s");
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
    use super::commands::{detect_research_intent, should_search};
    use super::language::{is_direct_answer_request, is_mixed_language_reply};

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
