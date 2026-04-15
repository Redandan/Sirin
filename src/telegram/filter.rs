//! Message filter — decide whether an incoming Telegram message should be
//! handled by the auto-reply pipeline.
//!
//! Shared between `run_listener_once` (legacy single-agent) and
//! `run_agent_listener_once` (per-agent) so both paths apply identical rules.

use chrono::{DateTime, Utc};
use grammers_client::message::Message;
use grammers_session::types::{PeerId, PeerKind};

use super::config::TelegramConfig;

/// Result of filtering an incoming message.
pub(super) enum FilterDecision {
    /// Skip the message; payload is a short reason for optional debug logging.
    Skip(&'static str),
    /// Handle the message; payload carries the already-extracted text and
    /// routing hints so callers don't need to re-derive them.
    Handle {
        text: String,
        is_private: bool,
        peer_bare_id: i64,
    },
}

/// Apply the standard auto-reply filter to a Telegram message.
///
/// Rules (in order):
/// 1. Skip outgoing messages.
/// 2. Skip messages sent by the logged-in user themselves (self-chat loop guard).
/// 3. Skip private DMs when `reply_private` is disabled.
/// 4. Skip group/channel messages when `reply_groups` is disabled.
/// 5. Skip group/channel messages not in `group_ids` when the allow-list is non-empty.
/// 6. Skip messages older than `listener_started_at` (avoids replaying backlog on reconnect).
/// 7. Skip empty-text messages (no content to reply to).
pub(super) fn filter_message(
    message: &Message,
    cfg: &TelegramConfig,
    listener_started_at: DateTime<Utc>,
) -> FilterDecision {
    if message.outgoing() {
        return FilterDecision::Skip("outgoing message");
    }
    if message.sender_id() == Some(PeerId::self_user()) {
        return FilterDecision::Skip("sender is self_user");
    }

    let is_private = matches!(
        message.peer_id().kind(),
        PeerKind::User | PeerKind::UserSelf
    );

    if is_private && !cfg.reply_private {
        return FilterDecision::Skip("private replies disabled");
    }
    if !is_private && !cfg.reply_groups {
        return FilterDecision::Skip("group replies disabled");
    }

    if !is_private && !cfg.group_ids.is_empty() {
        let peer_id = message.peer_id().bare_id();
        if !cfg.group_ids.contains(&peer_id) {
            return FilterDecision::Skip("group id not in TG_GROUP_IDS");
        }
    }

    if message.date() < listener_started_at {
        return FilterDecision::Skip("message older than listener start");
    }

    let text = message.text().to_owned();
    if text.is_empty() {
        return FilterDecision::Skip("empty text message");
    }

    FilterDecision::Handle {
        text,
        is_private,
        peer_bare_id: message.peer_id().bare_id(),
    }
}
