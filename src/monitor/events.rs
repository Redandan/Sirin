//! Monitor event types — ServerEvent and ClientCommand.
//!
//! `ServerEvent` is the stream emitted by Sirin toward any observer
//! (egui UI, WebSocket clients, NDJSON trace file).
//!
//! `ClientCommand` is the inverse: messages from an observer back to
//! the Sirin control plane (pause, resume, authz decisions, etc.).
//!
//! Both enums use internally-tagged serde (`#[serde(tag = "type")]`)
//! to match the TypeScript schema in DESIGN_MONITOR §5 exactly.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── ServerEvent ───────────────────────────────────────────────────────────────

/// Every event that Sirin can emit to observers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Sent once on connection/init — identifies the Sirin version and
    /// currently connected client IDs.
    Hello {
        ts: DateTime<Utc>,
        sirin_version: String,
        clients: Vec<String>,
    },

    /// An action is about to execute.
    ActionStart {
        /// Unique action invocation ID (UUID or monotonic counter as string).
        id: String,
        ts: DateTime<Utc>,
        /// MCP client identifier that requested the action.
        client: String,
        /// Action name (e.g. `"ax_click"`, `"navigate"`).
        action: String,
        /// Full args as a JSON value.
        args: serde_json::Value,
    },

    /// An action finished successfully.
    ActionDone {
        id: String,
        ts: DateTime<Utc>,
        result: serde_json::Value,
        duration_ms: u64,
    },

    /// An action finished with an error.
    ActionError {
        id: String,
        ts: DateTime<Utc>,
        error: String,
    },

    /// A human authorisation is required before proceeding.
    AuthzAsk {
        request_id: String,
        ts: DateTime<Utc>,
        client: String,
        action: String,
        args: serde_json::Value,
        url: String,
        timeout_ms: u64,
        learn: bool,
    },

    /// The outstanding authz ask was resolved (allowed or denied).
    AuthzResolved {
        request_id: String,
        ts: DateTime<Utc>,
        decision: String,
    },

    /// A browser screenshot frame (JPEG, base64-encoded).
    Screenshot {
        ts: DateTime<Utc>,
        /// Base64-encoded JPEG bytes.
        jpeg_base64: String,
    },

    /// The browser navigated to a new URL.
    UrlChange {
        ts: DateTime<Utc>,
        url: String,
    },

    /// A browser console log entry.
    Console {
        ts: DateTime<Utc>,
        /// Log level: `"log"`, `"warn"`, `"error"`, `"info"`, `"debug"`.
        level: String,
        text: String,
    },

    /// A network request/response pair captured by CDP.
    Network {
        ts: DateTime<Utc>,
        url: String,
        method: String,
        status: u16,
        size: u64,
        req_body_preview: Option<String>,
        res_body_preview: Option<String>,
    },

    /// Snapshot of the current control state (pause/step/abort flags).
    State {
        ts: DateTime<Utc>,
        paused: bool,
        step: bool,
        aborted: bool,
        view_active: bool,
    },

    /// Sent when the Sirin session ends or the WS connection is closing.
    Goodbye {
        ts: DateTime<Utc>,
    },
}

impl ServerEvent {
    /// Convenience: return the timestamp embedded in any variant.
    pub fn ts(&self) -> DateTime<Utc> {
        match self {
            ServerEvent::Hello { ts, .. } => *ts,
            ServerEvent::ActionStart { ts, .. } => *ts,
            ServerEvent::ActionDone { ts, .. } => *ts,
            ServerEvent::ActionError { ts, .. } => *ts,
            ServerEvent::AuthzAsk { ts, .. } => *ts,
            ServerEvent::AuthzResolved { ts, .. } => *ts,
            ServerEvent::Screenshot { ts, .. } => *ts,
            ServerEvent::UrlChange { ts, .. } => *ts,
            ServerEvent::Console { ts, .. } => *ts,
            ServerEvent::Network { ts, .. } => *ts,
            ServerEvent::State { ts, .. } => *ts,
            ServerEvent::Goodbye { ts } => *ts,
        }
    }
}

// ── ClientCommand ─────────────────────────────────────────────────────────────

/// Commands that an observer can send back to Sirin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    /// Resolve a pending authz ask.
    AuthzResponse {
        request_id: String,
        decision: AuthzDecision,
    },
    /// Pause action execution (in-flight action finishes; next is blocked).
    Pause {},
    /// Resume from a paused state.
    Resume {},
    /// Execute the next single action, then auto-pause again.
    Step {},
    /// Immediately reject all subsequent actions for this session.
    Abort {},
    /// Subscribe/unsubscribe from specific event channels (WS only).
    Subscribe {
        channels: Vec<SubscribeChannel>,
    },
}

/// The set of decisions available when resolving an authz ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthzDecision {
    AllowOnce,
    AllowAlwaysUrl,
    AllowAlwaysAction,
    Deny,
    DenyBlock,
}

/// Named event channels used by the `Subscribe` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscribeChannel {
    Screenshot,
    Action,
    Network,
    Console,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::{from_str, to_string};

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    /// Round-trip helper: serialize → deserialize → serialize again and
    /// compare the two JSON strings.
    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + std::fmt::Debug>(v: &T) {
        let json = to_string(v).expect("serialize failed");
        let back: T = from_str(&json).expect("deserialize failed");
        let json2 = to_string(&back).expect("re-serialize failed");
        assert_eq!(json, json2, "round-trip mismatch for {:?}", v);
    }

    #[test]
    fn hello_round_trip() {
        round_trip(&ServerEvent::Hello {
            ts: now(),
            sirin_version: "0.1.0".into(),
            clients: vec!["claude-desktop".into()],
        });
    }

    #[test]
    fn action_start_round_trip() {
        round_trip(&ServerEvent::ActionStart {
            id: "abc-123".into(),
            ts: now(),
            client: "claude-code".into(),
            action: "ax_click".into(),
            args: serde_json::json!({ "backend_id": 42 }),
        });
    }

    #[test]
    fn action_done_round_trip() {
        round_trip(&ServerEvent::ActionDone {
            id: "abc-123".into(),
            ts: now(),
            result: serde_json::json!({ "status": "ok" }),
            duration_ms: 38,
        });
    }

    #[test]
    fn action_error_round_trip() {
        round_trip(&ServerEvent::ActionError {
            id: "abc-123".into(),
            ts: now(),
            error: "element not found".into(),
        });
    }

    #[test]
    fn authz_ask_round_trip() {
        round_trip(&ServerEvent::AuthzAsk {
            request_id: "req-1".into(),
            ts: now(),
            client: "claude-desktop".into(),
            action: "navigate".into(),
            args: serde_json::json!({ "url": "https://example.com" }),
            url: "https://example.com".into(),
            timeout_ms: 30_000,
            learn: true,
        });
    }

    #[test]
    fn authz_resolved_round_trip() {
        round_trip(&ServerEvent::AuthzResolved {
            request_id: "req-1".into(),
            ts: now(),
            decision: "allow_once".into(),
        });
    }

    #[test]
    fn screenshot_round_trip() {
        round_trip(&ServerEvent::Screenshot {
            ts: now(),
            jpeg_base64: "AAAA".into(),
        });
    }

    #[test]
    fn url_change_round_trip() {
        round_trip(&ServerEvent::UrlChange {
            ts: now(),
            url: "https://app.example.com/wallet".into(),
        });
    }

    #[test]
    fn console_round_trip() {
        round_trip(&ServerEvent::Console {
            ts: now(),
            level: "error".into(),
            text: "Uncaught TypeError: null is not an object".into(),
        });
    }

    #[test]
    fn network_round_trip() {
        round_trip(&ServerEvent::Network {
            ts: now(),
            url: "https://api.example.com/v1/users".into(),
            method: "POST".into(),
            status: 200,
            size: 1024,
            req_body_preview: Some(r#"{"user":"alice"}"#.into()),
            res_body_preview: None,
        });
    }

    #[test]
    fn state_round_trip() {
        round_trip(&ServerEvent::State {
            ts: now(),
            paused: false,
            step: false,
            aborted: false,
            view_active: true,
        });
    }

    #[test]
    fn goodbye_round_trip() {
        round_trip(&ServerEvent::Goodbye { ts: now() });
    }

    // ── ClientCommand round-trips ────────────────────────────────────────────

    #[test]
    fn authz_response_round_trip() {
        round_trip(&ClientCommand::AuthzResponse {
            request_id: "req-1".into(),
            decision: AuthzDecision::AllowOnce,
        });
    }

    #[test]
    fn pause_round_trip() {
        round_trip(&ClientCommand::Pause {});
    }

    #[test]
    fn resume_round_trip() {
        round_trip(&ClientCommand::Resume {});
    }

    #[test]
    fn step_round_trip() {
        round_trip(&ClientCommand::Step {});
    }

    #[test]
    fn abort_round_trip() {
        round_trip(&ClientCommand::Abort {});
    }

    #[test]
    fn subscribe_round_trip() {
        round_trip(&ClientCommand::Subscribe {
            channels: vec![SubscribeChannel::Screenshot, SubscribeChannel::Action],
        });
    }

    #[test]
    fn type_tag_present_in_json() {
        // Verify that the `type` discriminant is present and lowercase_snake
        let json = to_string(&ServerEvent::UrlChange {
            ts: now(),
            url: "https://x.com".into(),
        })
        .unwrap();
        assert!(json.contains(r#""type":"url_change""#), "missing type tag: {json}");

        let json = to_string(&ClientCommand::Pause {}).unwrap();
        assert!(json.contains(r#""type":"pause""#), "missing type tag: {json}");
    }
}
