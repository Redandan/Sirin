//! `tracing::Layer` that tees formatted events into the [`log_buffer`] ring
//! so the UI log view keeps working unchanged after the tracing migration.
//!
//! ## How it fits
//! `main.rs` installs a subscriber with two layers: the standard stderr
//! formatter, plus [`LogBufferLayer`] which renders each event to a string
//! and pushes it through [`log_buffer::push`].  The existing `sirin_log!`
//! macro now expands to `tracing::info!` — all call sites still work, but
//! new code can use `tracing::info!` / `warn!` / `error!` directly, plus
//! `info_span!` around async tasks for correlation.
//!
//! Filter via `RUST_LOG=sirin=info,reqwest=warn` env var.

use std::fmt::Write;

use tracing::{field::Visit, Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::log_buffer;

pub struct LogBufferLayer;

impl<S: Subscriber> Layer<S> for LogBufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();

        // Skip noisy framework internals — only keep events from our own modules
        // + the crates we care about (reqwest/grammers/tokio get separate env filter).
        if !meta.target().starts_with("sirin") {
            return;
        }

        let level = meta.level();
        let mut msg = MessageVisitor::default();
        event.record(&mut msg);
        if msg.message.is_empty() {
            return;
        }

        // Match the pre-tracing sirin_log! format so UI log parsing (which
        // greps for `[telegram]` / `[ERROR]` etc.) continues to work.
        let prefix = match *level {
            Level::ERROR => "[ERROR] ",
            Level::WARN => "[WARN] ",
            _ => "",
        };
        log_buffer::push(format!("{prefix}{}", msg.message));
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(&mut self.message, "{value:?}");
            // Strip the leading quote added by the Debug impl of &str literals so
            // `info!("foo")` renders as `foo` not `"foo"`.
            if self.message.starts_with('"') && self.message.ends_with('"') {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        }
    }
}
