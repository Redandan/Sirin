//! Teams Web 瀏覽器自動化客戶端（事件驅動版）。
//!
//! 使用 CDP `Runtime.addBinding` + JS `MutationObserver`，
//! DOM 出現新未讀徽章時立即通知 Rust，無輪詢。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use headless_chrome::protocol::cdp::types::Event;
use headless_chrome::protocol::cdp::Runtime::AddBinding;
use headless_chrome::{Browser, LaunchOptions, Tab};

const SEL_MSG_INPUT: &str = "[data-tid='ckeditor']";
const TEAMS_URL: &str     = "https://teams.microsoft.com";
const BINDING_NAME: &str  = "sirinCallback";

/// JS 注入：MutationObserver 監聽未讀徽章變化，用 sirinCallback 通知 Rust。
const JS_OBSERVER: &str = r#"
(function() {
    if (window.__sirinObserverActive) return;
    window.__sirinObserverActive = true;

    let lastSeen = new Set();

    function collectUnread() {
        const items = document.querySelectorAll("[data-tid='chat-list-item']");
        const result = [];
        for (const item of items) {
            if (!item.querySelector("[data-tid='chat-list-item-unread-count']")) continue;
            const convid  = item.getAttribute('data-convid') || '';
            const titleEl = item.querySelector("[data-tid='chat-list-item-title']");
            const title   = titleEl ? titleEl.innerText.trim() : '未知';
            if (convid) result.push({ convid, title });
        }
        return result;
    }

    const observer = new MutationObserver(() => {
        const unread = collectUnread();
        const newItems = unread.filter(u => !lastSeen.has(u.convid));
        if (newItems.length > 0) {
            newItems.forEach(u => lastSeen.add(u.convid));
            window.sirinCallback(JSON.stringify(newItems));
        }
    });

    observer.observe(document.body, {
        subtree: true, childList: true,
        attributes: true, attributeFilter: ['data-tid', 'class'],
    });

    console.log('[sirin] MutationObserver ready');
})()
"#;

// ── Session 狀態 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus { NotStarted, WaitingForLogin, Running, Error(String) }

static STATUS: std::sync::OnceLock<Mutex<SessionStatus>> = std::sync::OnceLock::new();
fn status_cell() -> &'static Mutex<SessionStatus> {
    STATUS.get_or_init(|| Mutex::new(SessionStatus::NotStarted))
}
pub fn session_status() -> SessionStatus { status_cell().lock().unwrap().clone() }
fn set_status(s: SessionStatus) { *status_cell().lock().unwrap() = s; }

// ── 資料結構 ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UnreadChat { pub chat_id: String, pub peer_name: String }

// ── TeamsClient ───────────────────────────────────────────────────────────────

pub struct TeamsClient {
    _browser: Browser,
    pub tab: Arc<Tab>,
}

impl TeamsClient {
    /// 啟動有頭 Chrome 並等待用戶登入（SSO/MFA）。
    pub fn launch_and_login() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        set_status(SessionStatus::WaitingForLogin);
        let browser = Browser::new(
            LaunchOptions::default_builder()
                .headless(false)
                .build()
                .map_err(|e| format!("LaunchOptions: {e}"))?,
        )?;
        let tab = browser.new_tab()?;
        tab.navigate_to(TEAMS_URL)?.wait_until_navigated()?;
        Ok(Self { _browser: browser, tab })
    }

    /// 等待 URL 離開 microsoftonline.com（最多 `timeout_secs` 秒）。
    pub fn wait_for_login(&self, timeout_secs: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if std::time::Instant::now() > deadline { return false; }
            let url = self.tab.get_url();
            if !url.contains("login.microsoftonline") && !url.contains("login.live") {
                set_status(SessionStatus::Running);
                return true;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// 安裝 CDP binding + MutationObserver。
    /// 返回 `sync_channel` receiver；每當 Teams 出現新未讀，receiver 收到一批 UnreadChat。
    pub fn install_event_listener(
        &self,
    ) -> Result<std::sync::mpsc::Receiver<Vec<UnreadChat>>, Box<dyn std::error::Error + Send + Sync>>
    {
        // 1. 啟用 Runtime CDP domain
        self.tab.enable_runtime()?;

        // 2. 註冊 JS → Rust binding
        self.tab.call_method(AddBinding {
            name: BINDING_NAME.to_string(),
            execution_context_id:   None,
            execution_context_name: None,
        })?;

        // 3. std::sync channel（CDP callback 在同步執行緒）
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<UnreadChat>>(32);

        // 4. add_event_listener 接受 Fn(&Event)（closure 自動實作 EventListener<Event>）
        self.tab.add_event_listener(Arc::new(move |event: &Event| {
            let Event::RuntimeBindingCalled(ev) = event else { return };
            if ev.params.name != BINDING_NAME { return; }

            let chats: Vec<UnreadChat> =
                serde_json::from_str::<Vec<serde_json::Value>>(&ev.params.payload)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|obj| Some(UnreadChat {
                        chat_id:   obj["convid"].as_str()?.to_string(),
                        peer_name: obj["title"].as_str().unwrap_or("未知").to_string(),
                    }))
                    .collect();

            if !chats.is_empty() {
                let _ = tx.try_send(chats);
            }
        }))?;

        // 5. 注入 MutationObserver
        self.tab.evaluate(JS_OBSERVER, false)?;

        Ok(rx)
    }

    /// 點進對話並讀取最新一則訊息文字。
    pub fn read_latest_message(&self, chat: &UnreadChat) -> Option<String> {
        let js = format!(
            r#"(function(){{
                for(const item of document.querySelectorAll("[data-tid='chat-list-item']")){{
                    if(item.getAttribute('data-convid')==='{}'){{item.click();return true;}}
                }}
                return false;
            }})()"#,
            chat.chat_id.replace('\'', "\\'")
        );
        self.tab.evaluate(&js, false).ok()?;
        std::thread::sleep(Duration::from_millis(600));

        self.tab.evaluate(r#"(function(){
            const bs=document.querySelectorAll("[data-tid='message-body-content']");
            return bs.length?bs[bs.length-1].innerText.trim():'';
        })()"#, false)
            .ok()
            .and_then(|v| v.value)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    }

    /// 在當前對話中送出訊息。
    pub fn send_message(&self, text: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let input = self.tab.find_element(SEL_MSG_INPUT)?;
        input.click()?;
        std::thread::sleep(Duration::from_millis(150));
        input.type_into(text)?;
        std::thread::sleep(Duration::from_millis(200));
        self.tab.press_key("Return")?;
        Ok(())
    }
}
