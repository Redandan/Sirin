//! Pending-reply queue read/write — load, approve, reject, edit draft.

use super::RealService;
use crate::ui_service::*;

pub(super) fn pending_count(_svc: &RealService, agent_id: &str) -> usize {
    crate::pending_reply::load_pending(agent_id)
        .into_iter()
        .filter(|r| r.status == crate::pending_reply::PendingStatus::Pending)
        .count()
}

pub(super) fn load_pending(_svc: &RealService, agent_id: &str) -> Vec<PendingReplyView> {
    crate::pending_reply::load_pending(agent_id)
        .into_iter()
        .filter(|r| r.status == crate::pending_reply::PendingStatus::Pending)
        .map(|r| PendingReplyView {
            id: r.id, agent_id: r.agent_id, peer_name: r.peer_name,
            original_message: r.original_message, draft_reply: r.draft_reply, created_at: r.created_at,
        })
        .collect()
}

pub(super) fn approve_reply(svc: &RealService, agent_id: &str, reply_id: &str) {
    crate::pending_reply::update_status(agent_id, reply_id, crate::pending_reply::PendingStatus::Approved);
    svc.push_toast(ToastLevel::Success, "已核准");
}

pub(super) fn reject_reply(_svc: &RealService, agent_id: &str, reply_id: &str) {
    crate::pending_reply::update_status(agent_id, reply_id, crate::pending_reply::PendingStatus::Rejected);
}

pub(super) fn edit_draft(_svc: &RealService, agent_id: &str, reply_id: &str, new_text: &str) {
    let mut replies = crate::pending_reply::load_pending(agent_id);
    if let Some(r) = replies.iter_mut().find(|r| r.id == reply_id) {
        r.draft_reply = new_text.to_string();
    }
    let _ = crate::pending_reply::save_pending(agent_id, &replies);
}
