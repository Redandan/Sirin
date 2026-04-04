//! Lightweight process-wide event bus for inter-agent communication.
//!
//! Any module can publish an [`AgentEvent`] with [`publish`] and subscribe to
//! receive future events with [`subscribe`].  Subscribers that fall behind are
//! automatically skipped (lagged receivers get an error they can ignore).
//!
//! # Example
//! ```ignore
//! // Publisher (e.g. researcher.rs):
//! events::publish(AgentEvent::ResearchCompleted { … });
//!
//! // Subscriber (e.g. followup.rs):
//! let mut rx = events::subscribe();
//! while let Ok(event) = rx.recv().await {
//!     match event { AgentEvent::ResearchCompleted { .. } => { … } }
//! }
//! ```

use std::sync::OnceLock;
use tokio::sync::broadcast;

// ── Event types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A background research task finished (success or failure).
    ResearchCompleted {
        topic: String,
        task_id: String,
        success: bool,
    },
    /// The planner decided the user's message requires deep research.
    ResearchRequested {
        topic: String,
        url: Option<String>,
    },
    /// The followup worker marked a task as needing attention.
    FollowupTriggered {
        source_timestamp: String,
    },
    /// Persona objectives were updated after reflection.
    PersonaUpdated {
        new_objectives: Vec<String>,
    },
}

// ── Internal bus ──────────────────────────────────────────────────────────────

fn bus() -> &'static broadcast::Sender<AgentEvent> {
    static TX: OnceLock<broadcast::Sender<AgentEvent>> = OnceLock::new();
    // Channel capacity: hold up to 64 events before slow subscribers are skipped.
    TX.get_or_init(|| broadcast::channel(64).0)
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Publish an event to all active subscribers.  Silently drops if no
/// subscribers are listening or if the channel is full.
pub fn publish(event: AgentEvent) {
    let _ = bus().send(event);
}

/// Subscribe to future events.  The returned receiver misses events published
/// before this call; handle [`broadcast::error::RecvError::Lagged`] if needed.
pub fn subscribe() -> broadcast::Receiver<AgentEvent> {
    bus().subscribe()
}
