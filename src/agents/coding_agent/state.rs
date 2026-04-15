//! Shared mutable state for the coding agent's run loop.
//!
//! The ReAct phase ([`super::react::run_react_iterations`]) and the
//! verification+autofix phase ([`super::verify::run_verify_and_autofix`])
//! both append to the same history, track the same set of modified files,
//! and accumulate the same error flags.  `RunState` packs those shared
//! fields into one struct so the phase functions can take `&mut RunState`
//! instead of half a dozen `&mut T` parameters each.
//!
//! Per-phase counters that never escape (stalled_iterations,
//! consecutive_patch_errors, file_read_cache, etc.) stay local to their
//! phase function — only cross-phase state lives here.

use super::verdict::HistoryEntry;

#[derive(Default)]
pub(super) struct RunState {
    /// Every ReAct iteration appends here.  The verify phase may seed a
    /// separate `fix_history` from the `pinned` entries but writes back to
    /// this vector's `files_modified` / error flags.
    pub(super) history: Vec<HistoryEntry>,
    /// Paths touched by `file_write` / `file_patch` / `plan_execute`.
    /// Used by auto-commit (finalize) and rollback.
    pub(super) files_modified: Vec<String>,
    /// Set on `action == "DONE"`, fail-fast abort, or salvaged non-JSON
    /// output.  Empty when the loop hits `max_iter` without DONE.
    pub(super) final_answer: String,
    /// Any write-tool call happened (even if it errored out).
    pub(super) attempted_write: bool,
    /// Any tool or LLM-parse error occurred.
    pub(super) had_tool_errors: bool,
    /// Preview of the most recent error — surfaced to the user in the
    /// followup reason when verification fails.
    pub(super) last_tool_error: Option<String>,
}
