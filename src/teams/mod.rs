//! Teams 瀏覽器整合模組。
//!
//! # 流程
//! 1. `run_poller()` 啟動後開啟可見 Chrome 視窗，讓用戶完成 SSO / MFA
//! 2. 登入完成後每 30 秒掃描未讀對話
//! 3. 新訊息 → LLM 草稿 → `append_pending` → UI「待確認」tab
//! 4. 用戶點「送出」後 `update_status(Approved)` → 下次輪詢送出

pub mod browser_client;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::time::{Duration, interval};

use crate::events::{publish, AgentEvent};
use crate::pending_reply::{PendingReply, PendingStatus, append_pending, update_status};

pub use browser_client::session_status;

/// 主要輪詢任務，由 `main.rs` 以 `rt.spawn(teams::run_poller())` 啟動。
pub async fn run_poller() {
    // 啟動瀏覽器並等待用戶登入（blocking → spawn_blocking）
    let client = match tokio::task::spawn_blocking(|| -> Option<browser_client::TeamsClient> {
        match browser_client::TeamsClient::launch_and_login() {
            Ok(c) => {
                eprintln!("[teams] Browser launched — waiting for login (5 min)");
                if c.wait_for_login(300) {
                    eprintln!("[teams] Login confirmed");
                    Some(c)
                } else {
                    eprintln!("[teams] Login timeout — Teams poller stopped");
                    None
                }
            }
            Err(e) => {
                eprintln!("[teams] Failed to launch browser: {e}");
                None
            }
        }
    })
    .await
    {
        Ok(Some(c)) => Arc::new(Mutex::new(c)),
        _ => return,
    };

    let mut ticker = interval(Duration::from_secs(30));
    // 每個 session 只為同一對話建一次草稿（de-dup）
    let mut seen: HashSet<String> = HashSet::new();

    loop {
        ticker.tick().await;

        // ── 掃描未讀訊息 ─────────────────────────────────────────────────────
        let c = Arc::clone(&client);
        let seen_snap = seen.clone();

        let new_msgs = tokio::task::spawn_blocking(move || {
            let guard = c.lock().unwrap();
            guard.scan_unread_chats()
                .into_iter()
                .filter(|ch| !seen_snap.contains(&ch.chat_id))
                .filter_map(|ch| {
                    let msg = guard.read_latest_message(&ch)?;
                    if msg.trim().is_empty() { return None; }
                    Some((ch, msg))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        for (chat, msg_text) in new_msgs {
            seen.insert(chat.chat_id.clone());

            let draft = generate_draft(&msg_text);
            let mut reply = PendingReply::new(
                "teams",
                "teams",
                None,
                chat.peer_name.clone(),
                msg_text,
                draft.clone(),
            );
            reply.chat_id = Some(chat.chat_id.clone());

            let preview: String = draft.chars().take(60).collect();
            append_pending(reply.clone());

            publish(AgentEvent::ReplyPendingApproval {
                agent_id: "teams".to_string(),
                pending_id: reply.id.clone(),
                peer_name: chat.peer_name.clone(),
                draft_preview: preview,
            });

            eprintln!("[teams] Queued draft for '{}'", chat.peer_name);
        }

        // ── 送出已核准的草稿 ─────────────────────────────────────────────────
        let approved: Vec<_> = crate::pending_reply::load_pending("teams")
            .into_iter()
            .filter(|r| r.status == PendingStatus::Approved)
            .collect();

        for reply in approved {
            let Some(chat_id) = reply.chat_id.clone() else { continue };
            let text = reply.draft_reply.clone();
            let reply_id = reply.id.clone();
            let c = Arc::clone(&client);

            tokio::task::spawn_blocking(move || {
                let guard = c.lock().unwrap();
                match guard.send_message(&text) {
                    Ok(()) => {
                        eprintln!("[teams] Sent reply to {chat_id}");
                        // 標記 Rejected（已送出，防止重送）— 沒有 Sent 狀態故用 Rejected
                        update_status("teams", &reply_id, PendingStatus::Rejected);
                    }
                    Err(e) => eprintln!("[teams] Send failed: {e}"),
                }
            });
        }
    }
}

// ── 草稿生成 ──────────────────────────────────────────────────────────────────

fn generate_draft(msg: &str) -> String {
    let lower = msg.to_lowercase();
    if lower.contains('?') || lower.contains('？') || lower.contains("嗎") || lower.contains("how") || lower.contains("what") || lower.contains("when") {
        "收到！我稍後確認後回覆你，請稍等 🙏".to_string()
    } else if lower.contains("urgent") || lower.contains("緊急") || lower.contains("asap") {
        "已收到，我現在處理並盡快回覆！".to_string()
    } else {
        "收到，稍後回覆！".to_string()
    }
}
