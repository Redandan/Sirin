//! Teams Web 瀏覽器自動化客戶端。
//!
//! 透過 headless_chrome 操作 teams.microsoft.com：
//! - 一次性手動登入後保留 session（cookie 存活數天）
//! - 每 30 秒掃描未讀訊息
//! - 送出訊息（在用戶確認後由 pending_reply 觸發）

use std::sync::{Arc, Mutex};
use std::time::Duration;

use headless_chrome::{Browser, LaunchOptions, Tab};

// ── Teams Web selectors ───────────────────────────────────────────────────────
// These may need updating if Teams Web changes its DOM structure.

/// 未讀對話列表項目（sidebar chat list）
const SEL_CHAT_ITEM: &str = "[data-tid='chat-list-item']";
/// 未讀徽章（紅點/數字）
const SEL_UNREAD_BADGE: &str = "[data-tid='chat-list-item-unread-count']";
/// 訊息輸入框
const SEL_MSG_INPUT: &str = "[data-tid='ckeditor']";
/// 最新訊息氣泡文字
const SEL_MSG_BUBBLE: &str = "[data-tid='message-body-content']";
/// 對話人名稱
const SEL_CHAT_TITLE: &str = "[data-tid='chat-header-title']";

const TEAMS_URL: &str = "https://teams.microsoft.com";

// ── Session 狀態 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    /// 瀏覽器尚未啟動
    NotStarted,
    /// 等待用戶手動登入
    WaitingForLogin,
    /// 已登入，正常運行
    Running,
    /// 錯誤（訊息）
    Error(String),
}

// Process-wide session status（供 UI 查詢）
static SESSION_STATUS: std::sync::OnceLock<Mutex<SessionStatus>> = std::sync::OnceLock::new();

fn status_cell() -> &'static Mutex<SessionStatus> {
    SESSION_STATUS.get_or_init(|| Mutex::new(SessionStatus::NotStarted))
}

pub fn session_status() -> SessionStatus {
    status_cell().lock().unwrap().clone()
}

fn set_status(s: SessionStatus) {
    *status_cell().lock().unwrap() = s;
}

// ── TeamsClient ───────────────────────────────────────────────────────────────

pub struct TeamsClient {
    _browser: Browser,
    tab: Arc<Tab>,
}

impl TeamsClient {
    /// 啟動有頭（visible）Chrome，讓用戶完成登入。
    /// `headless(false)` 讓用戶看到視窗並手動通過 SSO / MFA。
    pub fn launch_and_login() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        set_status(SessionStatus::WaitingForLogin);

        let browser = Browser::new(
            LaunchOptions::default_builder()
                .headless(false)   // 顯示瀏覽器讓用戶登入
                .build()
                .map_err(|e| format!("LaunchOptions: {e}"))?,
        )?;

        let tab = browser.new_tab()?;
        tab.navigate_to(TEAMS_URL)?.wait_until_navigated()?;

        Ok(Self { _browser: browser, tab })
    }

    /// 等待登入完成（URL 離開 login.microsoftonline.com）。
    /// 最多等 `timeout_secs` 秒。
    pub fn wait_for_login(&self, timeout_secs: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if std::time::Instant::now() > deadline {
                return false;
            }
            let url = self.tab.get_url();
            if !url.contains("login.microsoftonline") && !url.contains("login.live") {
                set_status(SessionStatus::Running);
                return true;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// 掃描聊天列表，返回有未讀訊息的對話（(chat_element_id, sender_hint)）。
    pub fn scan_unread_chats(&self) -> Vec<UnreadChat> {
        let mut result = Vec::new();

        // 取得所有聊天列表項目
        let items = match self.tab.find_elements(SEL_CHAT_ITEM) {
            Ok(els) => els,
            Err(_) => return result,
        };

        for item in items {
            // 有未讀徽章？
            let has_unread = item
                .find_element(SEL_UNREAD_BADGE)
                .is_ok();
            if !has_unread {
                continue;
            }

            // 取得聊天標題作為 peer_name
            let peer_name = item
                .find_element("[data-tid='chat-list-item-title']")
                .ok()
                .and_then(|el| el.get_inner_text().ok())
                .unwrap_or_else(|| "未知".to_string());

            // 用 data attribute 或位置當做穩定 ID
            let chat_id = item
                .get_attribute_value("data-convid")
                .ok()
                .flatten()
                .unwrap_or_else(|| peer_name.clone());

            result.push(UnreadChat { chat_id, peer_name });
        }

        result
    }

    /// 點進指定對話，讀取最新一則訊息文字。
    pub fn read_latest_message(&self, chat: &UnreadChat) -> Option<String> {
        // 點擊對話項目
        let items = self.tab.find_elements(SEL_CHAT_ITEM).ok()?;
        let target = items.iter().find(|el| {
            el.get_attribute_value("data-convid")
                .ok()
                .flatten()
                .as_deref() == Some(&chat.chat_id)
        })?;
        target.click().ok()?;
        std::thread::sleep(Duration::from_millis(800));

        // 取最後一則訊息氣泡
        let bubbles = self.tab.find_elements(SEL_MSG_BUBBLE).ok()?;
        let last = bubbles.last()?;
        last.get_inner_text().ok()
    }

    /// 在目前開啟的對話中送出文字訊息。
    pub fn send_message(&self, text: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let input = self.tab.find_element(SEL_MSG_INPUT)?;
        input.click()?;
        std::thread::sleep(Duration::from_millis(200));

        // type_into 逐字元輸入（避免貼上觸發特殊行為）
        input.type_into(text)?;
        std::thread::sleep(Duration::from_millis(300));

        // Enter 送出
        self.tab.press_key("Return")?;
        Ok(())
    }
}

// ── 資料結構 ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UnreadChat {
    /// Teams 內部對話 ID（data-convid）
    pub chat_id: String,
    /// 顯示名稱（對方姓名或群組名）
    pub peer_name: String,
}
