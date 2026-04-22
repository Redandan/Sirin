//! Third-party integrations that are not part of Sirin's core test runner.
//!
//! Everything under this module is optional at runtime and must not be
//! required by the test runner.  It exists to support the **Assistant mode**
//! (see `src/assistant/`) where Sirin drives the user's own Chrome window
//! via a vendor extension — a distinct use case from headless CDP testing.

pub mod open_claude;
