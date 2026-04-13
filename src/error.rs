//! Unified error type for the Sirin codebase.
//!
//! Replaces scattered `Box<dyn Error>`, `Result<T, String>`, and ad-hoc error patterns
//! with a single `SirinError` enum.  Existing code can migrate gradually — the old
//! patterns still compile, and this module provides `From` conversions for easy adoption.

#[derive(Debug, thiserror::Error)]
pub enum SirinError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

impl From<String> for SirinError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

impl From<&str> for SirinError {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

/// Convenience alias used throughout the codebase.
pub type Result<T> = std::result::Result<T, SirinError>;
