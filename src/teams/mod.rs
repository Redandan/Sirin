//! Teams 整合：CDP 事件驅動，零輪詢。
//!
//! # 架構
//! ```text
//! Teams Web DOM 變化
//!     → MutationObserver (JS)
//!     → window.sirinCallback(json)
//!     → CDP Runtime.BindingCalled
//!     → std::sync::mpsc::SyncSender  (CDP thread)
//!     → tokio::sync::mpsc (bridge)
//!     → run_listener() async task
//!     → 立即送「稍等」+ 建草稿
//! ```
//!
//! # P1：核准即時送出
//! `notify_approved(reply_id)` 將草稿 ID 推入 `APPROVE_TX`；
//! `run_poller()` 的 `select!` 收到後立即發送，不等 60 秒定時器。

pub mod browser_client;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::mpsc as tmpsc;
use tokio::time::Duration;

use crate::events::{publish, AgentEvent};
use crate::pending_reply::{PendingReply, PendingStatus, append_pending, update_status};


// ── P1：即時核准通道 ──────────────────────────────────────────────────────────

static APPROVE_TX: OnceLock<tmpsc::Sender<String>> = OnceLock::new();

/// UI 核准草稿後呼叫此函數，`run_poller()` 將立即發送而非等待 60 秒。
pub fn notify_approved(reply_id: String) {
    if let Some(tx) = APPROVE_TX.get() {
        let _ = tx.try_send(reply_id);
    }
}

// ── 「稍等」模板 ──────────────────────────────────────────────────────────────

fn pick_ack(msg: &str) -> &'static str {
    let lower = msg.to_lowercase();
    if ["urgent", "緊急", "asap", "急"].iter().any(|k| lower.contains(k)) {
        "已收到，我現在處理，盡快回覆！"
    } else if ['?', '？'].iter().any(|c| lower.contains(*c))
        || ["嗎", "how", "what", "when", "who", "why", "where"]
            .iter().any(|k| lower.contains(k))
    {
        "收到！稍等一下，我確認後回覆你 🙏"
    } else {
        "收到！稍後回覆你 🙏"
    }
}

// ── 主要監聽任務 ──────────────────────────────────────────────────────────────

pub async fn run_poller() {
    // ── P1：建立即時核准通道 ──────────────────────────────────────────────────
    let (approve_tx, mut approve_rx) = tmpsc::channel::<String>(32);
    // store sender so notify_approved() can reach it
    let _ = APPROVE_TX.set(approve_tx);

    // ── 啟動瀏覽器 + 等待登入 ────────────────────────────────────────────────
    let (client, sync_rx) = match tokio::task::spawn_blocking(|| {
        let c = browser_client::TeamsClient::launch_and_login()
            .map_err(|e| format!("launch: {e}"))?;
        eprintln!("[teams] Browser open — please log in (5 min timeout)");
        if !c.wait_for_login(300) {
            return Err("login timeout".to_string());
        }
        eprintln!("[teams] Logged in ✓  Installing event listener…");
        let rx = c.install_event_listener()
            .map_err(|e| format!("listener: {e}"))?;
        eprintln!("[teams] MutationObserver active — zero-poll mode");
        Ok((c, rx))
    }).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(e))   => { eprintln!("[teams] Setup failed: {e}"); return; }
        Err(e)       => { eprintln!("[teams] spawn_blocking: {e}"); return; }
    };

    let client = Arc::new(Mutex::new(client));

    // ── Bridge：std::sync::mpsc → tokio::sync::mpsc ──────────────────────────
    // CDP callback 在同步執行緒，tokio task 需要 async channel。
    let (async_tx, mut async_rx) =
        tokio::sync::mpsc::channel::<Vec<browser_client::UnreadChat>>(32);

    std::thread::spawn(move || {
        while let Ok(chats) = sync_rx.recv() {
            let _ = async_tx.blocking_send(chats);
        }
    });

    // 本 session 內已處理過的 chat_id（避免重複送稍等）
    let mut acked:   HashSet<String> = HashSet::new();
    let mut drafted: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            // ── 新未讀訊息事件（立即觸發）────────────────────────────────────
            Some(unread_chats) = async_rx.recv() => {
                for chat in unread_chats {
                    let need_ack    = !acked.contains(&chat.chat_id);
                    let need_draft  = !drafted.contains(&chat.chat_id);
                    if !need_ack && !need_draft { continue; }

                    // 讀取訊息內容
                    let c = Arc::clone(&client);
                    let chat_clone = chat.clone();
                    let msg = tokio::task::spawn_blocking(move || {
                        c.lock().unwrap().read_latest_message(&chat_clone)
                    }).await.unwrap_or(None).unwrap_or_default();

                    if msg.trim().is_empty() { continue; }

                    // 立即送「稍等」
                    let peer_name = chat.peer_name.clone();
                    if need_ack {
                        acked.insert(chat.chat_id.clone());
                        let ack       = pick_ack(&msg).to_string();
                        let c         = Arc::clone(&client);
                        let peer_log  = peer_name.clone();
                        tokio::task::spawn_blocking(move || {
                            match c.lock().unwrap().send_message(&ack) {
                                Ok(())  => eprintln!("[teams] Auto-ack → '{peer_log}'"),
                                Err(e)  => eprintln!("[teams] Auto-ack failed: {e}"),
                            }
                        });
                    }

                    // 建立需確認的實質草稿
                    if need_draft {
                        drafted.insert(chat.chat_id.clone());
                        let draft = generate_draft(&msg);
                        let mut reply = PendingReply::new(
                            "teams", "teams", None,
                            peer_name.clone(),
                            msg,
                            draft.clone(),
                        );
                        reply.chat_id = Some(chat.chat_id.clone());
                        let preview: String = draft.chars().take(60).collect();
                        append_pending(reply.clone());
                        publish(AgentEvent::ReplyPendingApproval {
                            agent_id:      "teams".to_string(),
                            pending_id:    reply.id,
                            peer_name:     peer_name.clone(),
                            draft_preview: preview,
                        });
                        eprintln!("[teams] Draft queued for '{peer_name}'");
                    }
                }
            }

            // ── P1：用戶點「核准」→ 立即送出，無需等 60 秒 ──────────────────
            Some(reply_id) = approve_rx.recv() => {
                send_one_approved(&client, &reply_id).await;
            }

            // ── 每 60 秒：清掃任何殘留的已核准草稿 ──────────────────────────
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                send_approved_replies(&client).await;
            }
        }
    }
}

// ── 送出單一已核准草稿（P1：即時）────────────────────────────────────────────

async fn send_one_approved(client: &Arc<Mutex<browser_client::TeamsClient>>, reply_id: &str) {
    let pending = crate::pending_reply::load_pending("teams");
    let Some(reply) = pending.into_iter().find(|r| r.id == reply_id && r.status == PendingStatus::Approved)
    else { return };

    let Some(chat_id) = reply.chat_id.clone() else { return };
    let text     = reply.draft_reply.clone();
    let id_owned = reply.id.clone();
    let c        = Arc::clone(client);

    tokio::task::spawn_blocking(move || {
        match c.lock().unwrap().send_message(&text) {
            Ok(()) => {
                update_status("teams", &id_owned, PendingStatus::Rejected);
                eprintln!("[teams] Instant send → {chat_id}");
            }
            Err(e) => eprintln!("[teams] Instant send failed: {e}"),
        }
    });
}

// ── 清掃殘留的已核准草稿（60 秒定時器）──────────────────────────────────────

async fn send_approved_replies(client: &Arc<Mutex<browser_client::TeamsClient>>) {
    let approved: Vec<_> = crate::pending_reply::load_pending("teams")
        .into_iter()
        .filter(|r| r.status == PendingStatus::Approved)
        .collect();

    for reply in approved {
        let Some(chat_id) = reply.chat_id.clone() else { continue };
        let text     = reply.draft_reply.clone();
        let reply_id = reply.id.clone();
        let c        = Arc::clone(client);

        tokio::task::spawn_blocking(move || {
            match c.lock().unwrap().send_message(&text) {
                Ok(()) => {
                    update_status("teams", &reply_id, PendingStatus::Rejected);
                    eprintln!("[teams] Approved reply sent → {chat_id}");
                }
                Err(e) => eprintln!("[teams] Send failed: {e}"),
            }
        });
    }
}

// ── 實質草稿生成 ──────────────────────────────────────────────────────────────

fn generate_draft(msg: &str) -> String {
    let lower = msg.to_lowercase();
    if lower.contains("作業") || lower.contains("報告") || lower.contains("deadline") {
        format!("關於「{}⋯」我確認進度後回覆你！",
            msg.chars().take(20).collect::<String>())
    } else if lower.contains("會議") || lower.contains("meeting") || lower.contains("時間") {
        "我看一下行事曆，確認後回覆你！".to_string()
    } else {
        "我確認後詳細回覆你，請稍候。".to_string()
    }
}
