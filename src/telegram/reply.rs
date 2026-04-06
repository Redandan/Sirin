use chrono::Utc;
use grammers_client::{message::Message, Client};

use crate::{
    llm::{call_prompt_stream, LlmConfig},
    memory::append_context,
    persona::{TaskEntry, TaskTracker},
    sirin_log,
};

use super::{commands::message_preview, config::TelegramConfig};

// ── Streaming reply ───────────────────────────────────────────────────────────

/// Minimum tokens accumulated before we edit the placeholder message.
#[allow(dead_code)]
const STREAM_EDIT_EVERY_TOKENS: usize = 20;

/// Generate a reply via streaming LLM and progressively edit a Telegram
/// placeholder message as tokens arrive.
///
/// Flow:
/// 1. Send "⏳ 思考中…" placeholder immediately (fast).
/// 2. Stream tokens from the LLM; edit the message every
///    `STREAM_EDIT_EVERY_TOKENS` tokens so the user sees the reply grow.
/// 3. Final edit with the complete text.
///
/// Falls back to `send_final_reply` if the placeholder send fails.
#[allow(dead_code)]
pub async fn send_streaming_reply(
    client: &Client,
    message: &Message,
    is_private: bool,
    prompt: String,
    llm: &LlmConfig,
    http: &reqwest::Client,
) -> String {
    // 1. Send placeholder.
    let placeholder_text = "⏳ 思考中…";
    let sent: Option<Message> = if is_private {
        if let Some(peer_ref) = message.peer_ref().await {
            client.send_message(peer_ref, placeholder_text).await.ok()
        } else {
            message.reply(placeholder_text).await.ok()
        }
    } else {
        message.reply(placeholder_text).await.ok()
    };

    // If we couldn't even send the placeholder, fall back to blocking path.
    let Some(sent_msg) = sent else {
        sirin_log!("[telegram:stream] Failed to send placeholder, falling back to blocking reply");
        let reply = crate::llm::call_prompt(http, llm, prompt)
            .await
            .unwrap_or_default();
        send_final_reply(client, message, is_private, &reply).await;
        return reply;
    };

    // 2. Stream tokens; edit every STREAM_EDIT_EVERY_TOKENS tokens.
    let (token_tx, mut token_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Spawn the streaming LLM call.
    let http_clone = http.clone();
    let llm_clone = llm.clone();
    let prompt_clone = prompt.clone();
    let generate_task = tokio::spawn(async move {
        call_prompt_stream(&http_clone, &llm_clone, prompt_clone, move |token| {
            let _ = token_tx.send(token);
        })
        .await
        .unwrap_or_default()
    });

    // Collect tokens and periodically edit the message.
    let mut accumulated = String::new();
    let mut since_last_edit: usize = 0;

    while let Some(token) = token_rx.recv().await {
        accumulated.push_str(&token);
        since_last_edit += token.len();

        if since_last_edit >= STREAM_EDIT_EVERY_TOKENS && !accumulated.trim().is_empty() {
            let preview = format!("{} ▍", accumulated.trim_end());
            if let Err(e) = sent_msg.edit(preview.as_str()).await {
                sirin_log!("[telegram:stream] Edit failed (non-fatal): {e}");
            }
            since_last_edit = 0;
        }
    }

    // 3. Await the full result and do a final clean edit.
    let full = generate_task.await.unwrap_or(accumulated);
    let final_text = full.trim().to_string();

    if !final_text.is_empty() {
        if let Err(e) = sent_msg.edit(final_text.as_str()).await {
            sirin_log!("[telegram:stream] Final edit failed: {e}");
        }
    }

    sirin_log!(
        "[telegram:stream] Streaming reply complete ({} chars)",
        final_text.len()
    );
    final_text
}

pub async fn send_startup_message(client: &Client, cfg: &TelegramConfig) {
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
                            sirin_log!(
                                "[telegram] Failed to resolve TG_STARTUP_TARGET '@{username}': {e}"
                            );
                            None
                        }
                    }
                } else {
                    me.to_ref().await
                };

                if let Some(peer_ref) = target_peer_ref {
                    let text = msg.replace(
                        "{time}",
                        &Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
                    );
                    if let Err(e) = client.send_message(peer_ref, text.as_str()).await {
                        sirin_log!("[telegram] Failed to send startup message: {e}");
                    } else if let Some(ref username) = cfg.startup_target {
                        sirin_log!("[telegram] Startup message sent to @{username}");
                    } else {
                        sirin_log!("[telegram] Startup message sent to self");
                    }
                } else if cfg.startup_target.is_some() {
                    sirin_log!("[telegram] Could not resolve startup target peer_ref, startup message skipped");
                } else {
                    sirin_log!(
                        "[telegram] Could not resolve self peer_ref, startup message skipped"
                    );
                }
            }
            Err(e) => sirin_log!("[telegram] get_me failed: {e}"),
        }
    } else {
        sirin_log!("[telegram] TG_STARTUP_MSG is not set, startup message disabled");
    }
}

pub async fn send_final_reply(
    client: &Client,
    message: &Message,
    is_private: bool,
    final_reply: &str,
) {
    if is_private {
        if let Some(peer_ref) = message.peer_ref().await {
            match client.send_message(peer_ref, final_reply).await {
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
}

pub fn persist_reply_context(user_text: &str, assistant_reply: &str, peer_bare_id: Option<i64>) {
    if let Err(e) = append_context(user_text, assistant_reply, peer_bare_id) {
        sirin_log!("[telegram] Failed to save context: {e}");
    }
}

pub fn record_ai_decision_if_needed(
    tracker: &TaskTracker,
    persona_name: &str,
    text: &str,
    should_record_ai_decision: bool,
) {
    if should_record_ai_decision {
        let entry = TaskEntry::ai_decision(persona_name, Some(message_preview(text, 140)));
        if let Err(e) = tracker.record(&entry) {
            sirin_log!("[telegram] Failed to record task entry: {e}");
        }
    }
}
