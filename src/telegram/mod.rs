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

mod commands;
mod config;
mod language;
mod llm;

use std::sync::Arc;

use chrono::Utc;
use grammers_client::client::UpdatesConfiguration;
use grammers_client::SignInError;
use grammers_client::{Client, SenderPool};
use grammers_client::update::Update;
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerId, PeerKind};

use crate::memory::{append_context, load_recent_context};
use crate::sirin_log;
use crate::persona::{Persona, TaskEntry, TaskTracker};
use crate::researcher;
use crate::skills::ddg_search;
use crate::telegram_auth::TelegramAuthState;

use commands::{detect_research_intent, execute_user_request, extract_search_query, message_preview, should_search};
use config::{require_login, session_path, TelegramConfig, AUTH_INPUT_TIMEOUT_SECS};
use language::{chinese_fallback_reply, contains_cjk, is_direct_answer_request, is_mixed_language_reply};
use llm::generate_ai_reply;
use crate::llm::LlmConfig;

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
    let llm = LlmConfig::from_env();
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

    // Send startup notification to self
    if let Some(ref msg) = cfg.startup_msg {
        sirin_log!("[telegram] TG_STARTUP_MSG is enabled");
        match client.get_me().await {
            Ok(me) => {
                sirin_log!(
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
                            sirin_log!("[telegram] TG_STARTUP_TARGET '@{username}' not found");
                            None
                        }
                        Err(e) => {
                            sirin_log!("[telegram] Failed to resolve TG_STARTUP_TARGET '@{username}': {e}");
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
                        sirin_log!("[telegram] Failed to send startup message: {e}");
                    } else {
                        if let Some(ref username) = cfg.startup_target {
                            sirin_log!("[telegram] Startup message sent to @{username}");
                        } else {
                            sirin_log!("[telegram] Startup message sent to self");
                        }
                    }
                } else {
                    if cfg.startup_target.is_some() {
                        sirin_log!("[telegram] Could not resolve startup target peer_ref, startup message skipped");
                    } else {
                        sirin_log!("[telegram] Could not resolve self peer_ref, startup message skipped");
                    }
                }
            }
            Err(e) => sirin_log!("[telegram] get_me failed: {e}"),
        }
    } else {
        sirin_log!("[telegram] TG_STARTUP_MSG is not set, startup message disabled");
    }

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

            // Check for research intent before normal command dispatch.
            let research_execution: Option<String> = if let Some((topic, url)) = detect_research_intent(&text) {
                let topic_clone = topic.clone();
                let url_clone = url.clone();
                // Capture peer and a new client handle so the background task can
                // notify the user when research completes.
                let notify_peer = message.peer_ref().await;
                let notify_handle = handle.clone();
                tokio::spawn(async move {
                    let task = researcher::run_research(topic_clone, url_clone).await;
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

            // Load recent conversation context (per-peer, not global).
            let context_block: Option<String> = match load_recent_context(5, peer_bare_id) {
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
                    sirin_log!("[telegram] Failed to load context: {e}");
                    None
                }
            };

            // Perform web search when the message looks like a question.
            let search_context: Option<String> = if !direct_answer_request && should_search(&text) {
                let query = extract_search_query(&llm_client, &llm, &text).await;
                sirin_log!("[telegram] Searching web for: {query}");
                match ddg_search(&query).await {
                    Ok(results) if !results.is_empty() => {
                        let formatted = results
                            .iter()
                            .take(3)
                            .map(|r| format!("- {}: {} ({})", r.title, r.snippet, r.url))
                            .collect::<Vec<_>>()
                            .join("\n");
                        sirin_log!("[telegram] Web search returned {} result(s)", results.len().min(3));
                        Some(formatted)
                    }
                    Ok(_) => {
                        sirin_log!("[telegram] Web search returned no results");
                        None
                    }
                    Err(e) => {
                        sirin_log!("[telegram] Web search failed: {e}");
                        None
                    }
                }
            } else {
                None
            };

            // Inject relevant past research into the reply prompt (keyword match).
            let memory_context: Option<String> = {
                let lower_text = text.to_lowercase();
                match researcher::list_research() {
                    Ok(tasks) => tasks
                        .into_iter()
                        .filter(|t| t.status == researcher::ResearchStatus::Done)
                        .filter(|t| {
                            t.topic
                                .to_lowercase()
                                .split_whitespace()
                                .filter(|w| w.len() > 2)
                                .any(|word| lower_text.contains(word))
                        })
                        .filter_map(|t| t.final_report)
                        .next()
                        .map(|r| r.chars().take(600).collect()),
                    Err(e) => {
                        sirin_log!("[telegram] Failed to load research memory: {e}");
                        None
                    }
                }
            };
            if memory_context.is_some() {
                sirin_log!("[telegram] Injecting past research context into reply prompt");
            }

            let ai_reply = match generate_ai_reply(
                &llm_client,
                &llm,
                persona.as_ref(),
                &text,
                execution_result.as_deref(),
                search_context.as_deref(),
                context_block.as_deref(),
                memory_context.as_deref(),
                direct_answer_request,
                false,
            )
            .await
            {
                Ok(v) if !v.trim().is_empty() => v,
                Ok(_) => fallback_reply.clone(),
                Err(e) => {
                    sirin_log!("[telegram] AI reply generation failed, fallback to template: {e}");
                    fallback_reply.clone()
                }
            };

            let final_reply = if contains_cjk(&text)
                && (!contains_cjk(&ai_reply) || is_mixed_language_reply(&ai_reply))
            {
                sirin_log!("[telegram] AI reply language mismatch/mixed output: retrying with forced Traditional Chinese");
                match generate_ai_reply(
                    &llm_client,
                    &llm,
                    persona.as_ref(),
                    &text,
                    execution_result.as_deref(),
                    search_context.as_deref(),
                    context_block.as_deref(),
                    memory_context.as_deref(),
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
                        Ok(_) => sirin_log!("[telegram] Auto-reply sent (AI path, private direct message)"),
                        Err(e) => {
                            sirin_log!("[telegram] Private direct send failed, fallback to reply: {e}");
                            if let Err(reply_err) = message.reply(final_reply).await {
                                sirin_log!("[telegram] Failed to auto-reply: {reply_err}");
                            } else {
                                sirin_log!("[telegram] Auto-reply sent (AI path, private reply fallback)");
                            }
                        }
                    }
                } else if let Err(reply_err) = message.reply(final_reply).await {
                    sirin_log!("[telegram] Private peer_ref missing and reply failed: {reply_err}");
                } else {
                    sirin_log!("[telegram] Private peer_ref missing, sent via reply fallback");
                }
            } else if let Err(e) = message.reply(final_reply).await {
                sirin_log!("[telegram] Failed to auto-reply: {e}");
            } else {
                sirin_log!("[telegram] Auto-reply sent (AI path, group reply)");
            }

            if let Err(e) = append_context(&text, &reply_for_context, peer_bare_id) {
                sirin_log!("[telegram] Failed to save context: {e}");
            }
        } else if cfg.debug_updates {
            sirin_log!("[telegram] skip: auto-reply disabled by TG_AUTO_REPLY");
        }

        if should_record_ai_decision {
            let entry = TaskEntry::ai_decision(
                persona_name,
                Some(message_preview(&text, 140)),
            );
            if let Err(e) = tracker.record(&entry) {
                sirin_log!("[telegram] Failed to record task entry: {e}");
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
