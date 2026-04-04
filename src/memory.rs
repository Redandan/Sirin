//! Persistent memory for Sirin.
//!
//! Two layers:
//!
//! 1. **Full-text memory store** (`memory_store` / `memory_search`)
//!    Append-only JSONL index at `data/memory/index.jsonl`.
//!    Search uses TF-IDF-style term scoring — no external embedding model needed.
//!
//! 2. **Conversation context** (`append_context` / `load_recent_context`)
//!    Per-peer JSONL ring-log of recent user↔assistant turns.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ── Memory store ──────────────────────────────────────────────────────────────

fn memory_index_path() -> std::path::PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return std::path::Path::new(&local_app_data)
            .join("Sirin")
            .join("memory")
            .join("index.jsonl");
    }
    std::path::Path::new("data").join("memory").join("index.jsonl")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryEntry {
    timestamp: String,
    source: String, // "research" | "conversation" | "manual"
    text: String,
}

/// Persist a text snippet to the memory index.
pub fn memory_store(text: &str, source: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let path = memory_index_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = MemoryEntry {
        timestamp: Utc::now().to_rfc3339(),
        source: source.to_string(),
        text: text.to_string(),
    };
    let line = serde_json::to_string(&entry)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Search the memory index using simple TF-IDF term scoring.
/// Returns up to `limit` most relevant snippets, best-match first.
pub fn memory_search(query: &str, limit: usize) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let path = memory_index_path();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let query_terms: Vec<String> = tokenize(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);

    let mut scored: Vec<(f64, String)> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<MemoryEntry>(&line) else {
            continue;
        };
        let score = score_entry(&entry.text, &query_terms);
        if score > 0.0 {
            scored.push((score, entry.text));
        }
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored.into_iter().take(limit).map(|(_, t)| t).collect())
}

/// Tokenize text into lowercase words (CJK chars split individually).
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if is_cjk(ch) {
                // Flush pending ASCII word first.
                if !word.is_empty() {
                    tokens.push(word.to_lowercase());
                    word.clear();
                }
                tokens.push(ch.to_string());
            } else {
                word.push(ch);
            }
        } else {
            if !word.is_empty() {
                tokens.push(word.to_lowercase());
                word.clear();
            }
        }
    }
    if !word.is_empty() {
        tokens.push(word.to_lowercase());
    }
    tokens
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0x3400..=0x4DBF // CJK Extension A
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0x3040..=0x309F // Hiragana
        | 0x30A0..=0x30FF // Katakana
    )
}

/// Simple TF score: sum of (term_frequency_in_doc) for each query term.
fn score_entry(text: &str, query_terms: &[String]) -> f64 {
    let doc_tokens = tokenize(text);
    let doc_len = doc_tokens.len().max(1) as f64;

    // Build term frequency map.
    let mut tf: HashMap<&str, usize> = HashMap::new();
    for t in &doc_tokens {
        *tf.entry(t.as_str()).or_insert(0) += 1;
    }

    query_terms
        .iter()
        .map(|q| tf.get(q.as_str()).copied().unwrap_or(0) as f64 / doc_len)
        .sum()
}

// ── Conversation context (per-peer) ──────────────────────────────────────────

fn context_log_path(peer_id: Option<i64>) -> std::path::PathBuf {
    let filename = match peer_id {
        Some(id) => format!("sirin_context_{id}.jsonl"),
        None => "sirin_context.jsonl".to_string(),
    };
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return std::path::Path::new(&local_app_data)
            .join("Sirin")
            .join("tracking")
            .join(&filename);
    }
    std::path::Path::new("data")
        .join("tracking")
        .join(&filename)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEntry {
    pub timestamp: String,
    pub user_msg: String,
    pub assistant_reply: String,
}

/// Append a conversation turn to the per-peer context log.
pub fn append_context(
    user_msg: &str,
    assistant_reply: &str,
    peer_id: Option<i64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = context_log_path(peer_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = ContextEntry {
        timestamp: Utc::now().to_rfc3339(),
        user_msg: user_msg.to_string(),
        assistant_reply: assistant_reply.to_string(),
    };
    let line = serde_json::to_string(&entry)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Load the most recent `limit` context entries for a specific peer.
pub fn load_recent_context(
    limit: usize,
    peer_id: Option<i64>,
) -> Result<Vec<ContextEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let path = context_log_path(peer_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);

    let mut ring: VecDeque<ContextEntry> = VecDeque::with_capacity(limit);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<ContextEntry>(&line) {
            if ring.len() == limit {
                ring.pop_front();
            }
            ring.push_back(entry);
        }
    }
    Ok(ring.into_iter().collect())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_ascii() {
        let tokens = tokenize("Hello world Rust");
        assert_eq!(tokens, vec!["hello", "world", "rust"]);
    }

    #[test]
    fn tokenize_cjk() {
        let tokens = tokenize("Rust 語言");
        assert!(tokens.contains(&"rust".to_string()));
        assert!(tokens.contains(&"語".to_string()));
        assert!(tokens.contains(&"言".to_string()));
    }

    #[test]
    fn score_matches_relevant() {
        let doc = "Rust async runtime uses tokio for scheduling";
        let terms = tokenize("rust async");
        let score = score_entry(doc, &terms);
        assert!(score > 0.0, "should match");
    }

    #[test]
    fn score_zero_for_irrelevant() {
        let doc = "今天天氣很好";
        let terms = tokenize("rust async");
        let score = score_entry(doc, &terms);
        assert_eq!(score, 0.0);
    }
}
