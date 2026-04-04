use chrono::Utc;
use grammers_client::{message::Message, Client};

use crate::{
    memory::append_context,
    persona::{TaskEntry, TaskTracker},
    sirin_log,
};

use super::{commands::message_preview, config::TelegramConfig};

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
                            sirin_log!("[telegram] Failed to resolve TG_STARTUP_TARGET '@{username}': {e}");
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
                    sirin_log!("[telegram] Could not resolve self peer_ref, startup message skipped");
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
