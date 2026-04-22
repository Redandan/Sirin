//! Open Claude (Chrome extension) integration — Assistant mode only.
//!
//! This module connects Sirin to the open-claude-in-chrome extension via a
//! native messaging host.  It is **not** used by the test runner — the test
//! runner drives headless Chrome directly via CDP in `src/browser.rs`.
//!
//! Valid use cases (Assistant mode — see `src/assistant/`):
//!   - Driving the user's currently-open Chrome window (can't steal profile
//!     lock with CDP while Chrome is running)
//!   - Casual automation where the user watches and can take over
//!     (e.g. scraping Google Maps reviews, farming FB game tasks)
//!
//! Components:
//!   - `client` — TCP client talking to mcp-server.js on :18765
//!   - `bridge` — Chrome Native Messaging host (stdin/stdout framing)

pub mod bridge;
pub mod client;

#[allow(unused_imports)]
pub use client::{ComputerToolResult, OpenClaudeClient, OpenClaudeConfig};
