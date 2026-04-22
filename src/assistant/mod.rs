//! Assistant mode — Sirin drives the user's own Chrome window for casual
//! automation tasks (Google Maps review scraping, FB game farming, etc.).
//!
//! Distinct from Test mode (`test_runner`) in several ways:
//!   - Talks to Chrome via Open Claude extension (`integrations::open_claude`),
//!     not CDP — because the user's Chrome holds the profile lock.
//!   - Prefers cheap models (Haiku + vision) since tasks are repetitive and
//!     predictable.
//!   - Not headless, not parallel, not isolated — single user Chrome window.
//!
//! Current status: scaffold only.  Populate as concrete tasks are added.

// Intentionally empty — populate with task modules as they are added.
