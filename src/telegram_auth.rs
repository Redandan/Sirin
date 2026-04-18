//! Non-blocking Telegram authentication state shared between the
//! background listener task and Tauri UI commands.
//!
//! ## Concurrency
//! `TelegramAuthState` clones cheaply (inner `Arc<Mutex<Inner>>`).  All status
//! mutations go through the mutex; the code-input channel is a
//! `tokio::sync::mpsc` so the UI can inject user input without the listener
//! blocking a thread while it waits.
//!
//! Flow:
//! 1. `run_listener` detects an unauthorised session and calls
//!    `TelegramAuthState::request_code`.  This sets the status to
//!    `CodeRequired`, stores a one-shot sender, and returns a receiver
//!    the listener can `.await` on (with a timeout).
//! 2. The frontend polls `telegram_get_auth_status` and shows a code
//!    input when the status is `CodeRequired` or `PasswordRequired`.
//! 3. The user submits the code via `telegram_submit_auth_code`.  The
//!    Tauri command feeds it into the waiting receiver so the listener
//!    can continue without ever touching stdin.

use std::sync::Mutex;
use serde::Serialize;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use tokio::sync::{oneshot, Notify};

// ── Public status type ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TelegramStatus {
    /// Listener has not started yet (env vars absent, session not opened, …).
    Disconnected { reason: String },
    /// Listener is healthy and connected.
    Connected,
    /// App is waiting for the user to supply a login code.
    CodeRequired,
    /// App is waiting for the user to supply a 2-FA password.
    PasswordRequired { hint: String },
    /// Auth failed or a fatal error occurred.
    Error { message: String },
}

// ── Inner mutable state ───────────────────────────────────────────────────────

struct Inner {
    status: TelegramStatus,
    code_tx: Option<oneshot::Sender<String>>,
    password_tx: Option<oneshot::Sender<String>>,
}

impl Inner {
    fn new() -> Self {
        Self {
            status: TelegramStatus::Disconnected {
                reason: "not started".into(),
            },
            code_tx: None,
            password_tx: None,
        }
    }
}

// ── Shared handle ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TelegramAuthState {
    inner: Arc<Mutex<Inner>>,
    /// Fired by the UI to skip the backoff sleep and immediately retry.
    reconnect_notify: Arc<Notify>,
    /// Set by the UI when manually triggering reconnect; bypasses the
    /// `TG_REQUIRE_LOGIN` default-false gate so login actually runs.
    force_login: Arc<AtomicBool>,
}

impl TelegramAuthState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new())),
            reconnect_notify: Arc::new(Notify::new()),
            force_login: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal the background listener to skip its backoff sleep and reconnect now.
    /// Also sets the force-login flag so the auth flow is not skipped.
    pub fn trigger_reconnect(&self) {
        self.force_login.store(true, Ordering::Release);
        self.reconnect_notify.notify_one();
    }

    /// Return a reference to the notify so the listener can await it.
    pub fn reconnect_notified(&self) -> &Notify {
        &self.reconnect_notify
    }

    /// Consume and return the force-login flag.  Called once per connection attempt.
    pub fn take_force_login(&self) -> bool {
        self.force_login.swap(false, Ordering::AcqRel)
    }

    // ── Called from the listener task ─────────────────────────────────────────

    pub fn set_connected(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).status = TelegramStatus::Connected;
    }

    pub fn set_disconnected(&self, reason: impl Into<String>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).status = TelegramStatus::Disconnected {
            reason: reason.into(),
        };
    }

    pub fn set_error(&self, message: impl Into<String>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).status = TelegramStatus::Error {
            message: message.into(),
        };
    }

    /// Mark status as `CodeRequired` and return a receiver the caller can
    /// `.await` to get the code entered by the user.  Times out after
    /// `timeout_secs` seconds; returns `None` on timeout.
    pub async fn request_code(&self, timeout_secs: u64) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.status = TelegramStatus::CodeRequired;
            inner.code_tx = Some(tx);
        }
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx)
            .await
            .ok()
            .and_then(|r| r.ok())
    }

    /// Mark status as `PasswordRequired` and return a receiver similar to
    /// `request_code`.
    pub async fn request_password(
        &self,
        hint: impl Into<String>,
        timeout_secs: u64,
    ) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.status = TelegramStatus::PasswordRequired { hint: hint.into() };
            inner.password_tx = Some(tx);
        }
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx)
            .await
            .ok()
            .and_then(|r| r.ok())
    }

    // ── Called from Tauri commands ─────────────────────────────────────────────

    /// Return the current status (serialisable snapshot).
    pub fn status(&self) -> TelegramStatus {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).status.clone()
    }

    /// Feed a login code from the UI into the waiting listener.
    /// Returns `true` when there was a pending receiver, `false` otherwise.
    pub fn submit_code(&self, code: String) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = inner.code_tx.take() {
            let _ = tx.send(code);
            true
        } else {
            false
        }
    }

    /// Feed a 2-FA password from the UI.
    pub fn submit_password(&self, password: String) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = inner.password_tx.take() {
            let _ = tx.send(password);
            true
        } else {
            false
        }
    }
}
