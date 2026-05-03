//! Generate `config/test-schema.json` from `parser::TestGoal` (Issue #255).
//!
//! ## Usage
//!
//! ```bash
//! cargo run --bin gen-schema
//! ```
//!
//! Writes to repo-relative `config/test-schema.json`.  CI / unit test
//! `parser::tests::schema_has_no_drift` ensures the committed file stays
//! in sync with the struct definitions.
//!
//! ## How IDEs use the output
//!
//! Add to the top of any `config/tests/*.yaml`:
//!
//! ```yaml
//! # yaml-language-server: $schema=../test-schema.json
//! id: my_test
//! name: ...
//! ```
//!
//! VS Code's YAML extension (`redhat.vscode-yaml`) reads the comment and
//! provides auto-complete + validation for every field documented in the
//! schema, including enum constraints (locale, perception, viewport.mobile).
//!
//! ## Why this binary uses `#[path]` includes
//!
//! Sirin doesn't have a `lib.rs` — every internal module is declared from
//! `src/main.rs`.  Rather than introduce a library refactor for one
//! schema-generation tool, this binary uses `#[path = "..."]` to include
//! the parser module directly, plus minimal local stubs for the two
//! cross-module references parser.rs makes (`crate::perception` and
//! `crate::platform`).  A unit test in parser.rs verifies the stub
//! [`PerceptionMode`] keeps the same JSON shape as the real one.

use schemars::schema_for;

// Local stub for `crate::perception::PerceptionMode` — same #[serde] +
// #[derive(JsonSchema)] shape as the real type, INCLUDING doc comments
// (schemars picks them up as `description` fields).  Drift between this
// and the real type is caught by `parser::tests::schema_has_no_drift`.
mod perception {
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    /// How the executor should observe the page before each LLM turn.
    #[derive(
        Debug, Clone, Copy, PartialEq, Eq,
        Serialize, Deserialize, Default, JsonSchema,
    )]
    #[serde(rename_all = "lowercase")]
    pub enum PerceptionMode {
        /// Legacy text-only observation.  No screenshot, no vision LLM call.
        #[default]
        Text,
        /// Always attach a screenshot to the prompt and call the vision LLM.
        Vision,
        /// Detect canvas (Flutter / WebGL) at runtime; use Vision if detected,
        /// otherwise Text.
        Auto,
    }
}

// Local stub for `crate::platform::config_dir` — only used by parser
// helper fns we don't reach from `schema_for!`, but the symbol must
// resolve at compile time.
mod platform {
    use std::path::PathBuf;
    #[allow(dead_code)]
    pub fn config_dir() -> PathBuf {
        PathBuf::from("config")
    }
    #[allow(dead_code)]
    pub fn config_path(rel: impl AsRef<std::path::Path>) -> PathBuf {
        config_dir().join(rel)
    }
}

// Stub for the lint module — parser::find() calls `super::lint::lint(g)`.
// We never invoke `find()` from this binary (schema_for! reads the type
// definition only), but the symbol still needs to resolve.
#[allow(dead_code)]
mod lint {
    pub fn lint<T>(_g: &T) -> Vec<()> { Vec::new() }
    pub fn log_issues<T>(_g: &T, _issues: &[()]) {}
}

#[allow(dead_code, unused_imports)]
#[path = "../test_runner/parser.rs"]
mod parser;

fn main() -> std::io::Result<()> {
    let schema = schema_for!(parser::TestGoal);
    let json = serde_json::to_string_pretty(&schema)
        .expect("schema serialisation should not fail");
    let out_path = std::path::Path::new("config").join("test-schema.json");

    // Trailing newline so editors don't fight us on save.
    let content = format!("{json}\n");
    std::fs::write(&out_path, &content)?;
    println!(
        "[gen-schema] wrote {} ({} bytes)",
        out_path.display(),
        content.len(),
    );
    Ok(())
}
