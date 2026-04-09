//! Teams Web 瀏覽器自動化客戶端（事件驅動 + 持久化 Profile）。
//!
//! # 改進
//! - P0：`data/teams_profile` 持久化 Chrome profile，重啟免重新登入
//! - P2：`send_message` 改用 JS `execCommand` + 點擊 Send 按鈕，
//!        不依賴 UI 焦點，避免打字衝突

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use headless_chrome::protocol::cdp::types::Event;
use headless_chrome::protocol::cdp::Runtime::AddBinding;
use headless_chrome::{Browser, LaunchOptions, Tab};
use std::sync::Arc;

const TEAMS_URL: &str    = "https://teams.microsoft.com";
const BINDING_NAME: &str = "sirinCallback";

/// 持久化 Chrome profile 路徑（保存 cookie / session）。
fn profile_dir() -> PathBuf {
    PathBuf::from("data").join("teams_profile")
}

// ── JS：MutationObserver（新未讀 → sirinCallback）────────────────────────────
const JS_OBSERVER: &str = r#"
(function() {
    if (window.__sirinObserverActive) return;
    window.__sirinObserverActive = true;
    let lastSeen = new Set();

    function collectUnread() {
        const items = document.querySelectorAll(
            "[data-tid='chat-list-item'], [aria-label][data-convid]"
        );
        const result = [];
        for (const item of items) {
            const hasBadge = item.querySelector(
                "[data-tid='chat-list-item-unread-count'], [aria-label*='unread']"
            );
            if (!hasBadge) continue;
            const convid  = item.getAttribute('data-convid') || '';
            const titleEl = item.querySelector(
                "[data-tid='chat-list-item-title'], [aria-label]"
            );
            const title = titleEl
                ? (titleEl.innerText || titleEl.getAttribute('aria-label') || '未知').trim()
                : '未知';
            if (convid) result.push({ convid, title });
        }
        return result;
    }

    const observer = new MutationObserver(() => {
        const unread   = collectUnread();
        const newItems = unread.filter(u => !lastSeen.has(u.convid));
        if (newItems.length > 0) {
            newItems.forEach(u => lastSeen.add(u.convid));
            window.sirinCallback(JSON.stringify(newItems));
        }
    });

    observer.observe(document.body, {
        subtree: true, childList: true,
        attributes: true,
        attributeFilter: ['data-tid', 'class', 'aria-label'],
    });
    console.log('[sirin] MutationObserver ready (multi-selector)');
})()
"#;

/// JS 注入發送訊息（P2）：不使用 type_into，改用 execCommand + 點擊 Send 按鈕。
/// 不依賴當前 UI 焦點，背景執行安全。
fn js_send_message(text: &str) -> String {
    // JSON-encode the text so it's safe to embed in JS string literal.
    let text_json = serde_json::to_string(text).unwrap_or_else(|_| format!("\"{}\"", text));
    format!(r#"
(function() {{
    // 找到 CKEditor contenteditable 輸入框（多個 selector fallback）
    const input = document.querySelector(
        "[data-tid='ckeditor']," +
        "[contenteditable='true'][role='textbox']," +
        ".ck-editor__editable"
    );
    if (!input) {{ return 'NO_INPUT'; }}

    input.focus();

    // 清空並插入文字（execCommand 保留 React/Angular 的 input 事件）
    document.execCommand('selectAll', false, null);
    document.execCommand('delete', false, null);
    document.execCommand('insertText', false, {text_json});

    // 點擊 Send 按鈕（多個 selector fallback）
    const send = document.querySelector(
        "[data-tid='sendMessageCommands-send']," +
        "button[aria-label='Send']," +
        "button[aria-label='傳送']," +
        "button[aria-label='发送']"
    );
    if (send) {{
        send.click();
        return 'SENT_VIA_BUTTON';
    }}

    // Fallback：模擬 Enter 鍵事件
    input.dispatchEvent(new KeyboardEvent('keydown', {{
        key: 'Enter', code: 'Enter', keyCode: 13,
        bubbles: true, cancelable: true
    }}));
    return 'SENT_VIA_ENTER';
}})()
"#, text_json = text_json)
}

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
    /// 啟動有頭 Chrome，使用持久化 profile（`data/teams_profile`）。
    /// 若 cookie 有效則直接進入 Teams，否則等待用戶完成 SSO/MFA。
    pub fn launch_and_login() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        set_status(SessionStatus::WaitingForLogin);

        let profile = profile_dir();
        std::fs::create_dir_all(&profile)?;

        let browser = Browser::new(
            LaunchOptions::default_builder()
                .headless(false)
                .user_data_dir(Some(profile))   // P0：持久化 profile
                .build()
                .map_err(|e| format!("LaunchOptions: {e}"))?,
        )?;
        let tab = browser.new_tab()?;
        tab.navigate_to(TEAMS_URL)?.wait_until_navigated()?;
        Ok(Self { _browser: browser, tab })
    }

    /// 等待 URL 離開 microsoftonline.com（已有 session 時幾乎立即通過）。
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

    /// 安裝 CDP binding + MutationObserver，返回事件 receiver。
    pub fn install_event_listener(
        &self,
    ) -> Result<std::sync::mpsc::Receiver<Vec<UnreadChat>>, Box<dyn std::error::Error + Send + Sync>>
    {
        self.tab.enable_runtime()?;
        self.tab.call_method(AddBinding {
            name: BINDING_NAME.to_string(),
            execution_context_id:   None,
            execution_context_name: None,
        })?;

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<UnreadChat>>(32);

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
            if !chats.is_empty() { let _ = tx.try_send(chats); }
        }))?;

        self.tab.evaluate(JS_OBSERVER, false)?;
        Ok(rx)
    }

    /// 切換到指定對話並讀取最新一則訊息。
    pub fn read_latest_message(&self, chat: &UnreadChat) -> Option<String> {
        let js = format!(
            r#"(function(){{
                const sel = "[data-convid='{}']";
                const el  = document.querySelector(sel);
                if (el) {{ el.click(); return true; }}
                return false;
            }})()"#,
            chat.chat_id.replace('\'', "\\'")
        );
        self.tab.evaluate(&js, false).ok()?;
        std::thread::sleep(Duration::from_millis(600));

        self.tab.evaluate(r#"(function(){
            const bs = document.querySelectorAll(
                "[data-tid='message-body-content'], [class*='messageBody']"
            );
            return bs.length ? bs[bs.length-1].innerText.trim() : '';
        })()"#, false)
            .ok()
            .and_then(|v| v.value)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    }

    /// P2：JS 注入發送訊息（不依賴 UI 焦點，背景安全執行）。
    pub fn send_message(&self, text: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let js = js_send_message(text);
        let result = self.tab.evaluate(&js, false)?;
        let status = result.value
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        if status == "NO_INPUT" {
            return Err("Teams 輸入框未找到".into());
        }
        eprintln!("[teams] send_message status: {status}");
        Ok(())
    }
}
