//! Persistent memory for Sirin.
//!
//! Three layers:
//!
//! 1. **Full-text memory store** (`memory_store` / `memory_search`)
//!    Append-only JSONL index at `data/memory/index.jsonl`.
//!    Search uses lightweight term scoring — no external embedding model needed.
//!
//! 2. **Project codebase index** (`refresh_codebase_index` / `search_codebase`)
//!    Periodically scans the local repository and stores architecture-aware file summaries.
//!
//! 3. **Conversation context** (`append_context` / `load_recent_context`)
//!    Per-peer JSONL ring-log of recent user↔assistant turns.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::Utc;
use parking_lot::RwLock;
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

// ── In-memory cache ───────────────────────────────────────────────────────────

/// Global in-process cache of all memory entries.
/// `None` means not yet loaded from disk; `Some(vec)` is the live index.
fn memory_entry_cache() -> &'static RwLock<Option<Vec<MemoryEntry>>> {
    static CACHE: OnceLock<RwLock<Option<Vec<MemoryEntry>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(None))
}

/// Ensure the cache is populated from disk (no-op if already loaded).
fn ensure_cache_loaded() {
    // Fast path: already loaded.
    if memory_entry_cache().read().is_some() {
        return;
    }
    // Slow path: acquire write lock and load.
    let mut guard = memory_entry_cache().write();
    if guard.is_some() {
        return; // Another thread beat us.
    }
    let path = memory_index_path();
    let entries: Vec<MemoryEntry> = if path.exists() {
        fs::File::open(&path)
            .ok()
            .map(|f| {
                BufReader::new(f)
                    .lines()
                    .filter_map(|l| l.ok())
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str::<MemoryEntry>(&l).ok())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    *guard = Some(entries);
}

/// Persist a text snippet to the memory index.
pub fn memory_store(text: &str, source: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let entry = MemoryEntry {
        timestamp: Utc::now().to_rfc3339(),
        source: source.to_string(),
        text: text.to_string(),
    };
    // Write to disk first.
    let path = memory_index_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(&entry)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    // Keep cache in sync if it is already loaded.
    let mut guard = memory_entry_cache().write();
    if let Some(ref mut entries) = *guard {
        entries.push(entry);
    }
    Ok(())
}

/// Search the memory index using simple TF-IDF term scoring.
/// Results come from the in-process cache (loaded from disk on first call).
pub fn memory_search(query: &str, limit: usize) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let query_terms: Vec<String> = tokenize(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }

    ensure_cache_loaded();

    let guard = memory_entry_cache().read();
    let entries = guard.as_deref().unwrap_or(&[]);

    let mut scored: Vec<(f64, &str)> = entries
        .iter()
        .filter_map(|e| {
            let s = score_entry(&e.text, &query_terms);
            if s > 0.0 { Some((s, e.text.as_str())) } else { None }
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored.into_iter().take(limit).map(|(_, t)| t.to_owned()).collect())
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

// ── Project codebase index ───────────────────────────────────────────────────

fn codebase_index_path() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return Path::new(&local_app_data)
            .join("Sirin")
            .join("memory")
            .join("codebase_index.jsonl");
    }
    Path::new("data").join("memory").join("codebase_index.jsonl")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodebaseEntry {
    path: String,
    kind: String,
    summary: String,
    symbols: Vec<String>,
    text: String,
}

fn find_project_root() -> Option<PathBuf> {
    let mut current = std::env::current_dir().ok()?;
    loop {
        if current.join("Cargo.toml").exists() || current.join("tauri.conf.json").exists() {
            return Some(current);
        }
        if !current.pop() {
            break;
        }
    }
    None
}

fn code_file_kind(path: &Path) -> &'static str {
    match path.extension().and_then(|v| v.to_str()).unwrap_or_default() {
        "rs" => "rust-source",
        "toml" => "cargo-config",
        "md" => "documentation",
        "yaml" | "yml" => "yaml-config",
        "ts" | "tsx" => "frontend-source",
        "js" | "jsx" => "javascript-source",
        "json" => "json-config",
        _ => "text",
    }
}

fn is_codebase_candidate(path: &Path, metadata: &fs::Metadata) -> bool {
    if !metadata.is_file() || metadata.len() > 256_000 {
        return false;
    }

    matches!(
        path.extension().and_then(|v| v.to_str()).unwrap_or_default(),
        "rs" | "toml" | "md" | "yaml" | "yml" | "ts" | "tsx" | "js" | "jsx" | "json"
    )
}

fn collect_codebase_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            let name = path.file_name().and_then(|v| v.to_str()).unwrap_or_default();
            if matches!(name, ".git" | "target" | "node_modules" | ".next" | "dist" | "build") {
                continue;
            }
            collect_codebase_files(&path, out)?;
            continue;
        }

        if is_codebase_candidate(&path, &metadata) {
            out.push(path);
        }
    }
    Ok(())
}

fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .map(|line| {
            line.trim_start_matches("//!")
                .trim_start_matches("///")
                .trim_start_matches('#')
                .trim()
        })
        .find(|line| !line.is_empty())
        .unwrap_or("No summary available")
        .to_string()
}

fn capture_symbol_after_prefix(line: &str, prefix: &str) -> Option<String> {
    line.strip_prefix(prefix)
        .and_then(|rest| {
            rest.split(|c: char| c == '(' || c == '<' || c == '{' || c == ':' || c.is_whitespace())
                .next()
        })
        .filter(|name| !name.is_empty())
        .map(|name| name.trim_matches(',').to_string())
}

fn extract_symbols(path: &Path, text: &str) -> Vec<String> {
    let ext = path.extension().and_then(|v| v.to_str()).unwrap_or_default();
    let mut symbols = Vec::new();

    for line in text.lines().take(240) {
        let trimmed = line.trim();
        let candidate = match ext {
            "rs" => capture_symbol_after_prefix(trimmed, "pub async fn ")
                .or_else(|| capture_symbol_after_prefix(trimmed, "async fn "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "pub fn "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "fn "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "pub struct "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "struct "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "pub enum "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "enum "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "pub trait "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "trait "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "pub mod "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "mod ")),
            "ts" | "tsx" | "js" | "jsx" => capture_symbol_after_prefix(trimmed, "export async function ")
                .or_else(|| capture_symbol_after_prefix(trimmed, "export function "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "function "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "export const "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "const "))
                .or_else(|| capture_symbol_after_prefix(trimmed, "export default function ")),
            _ => None,
        };

        if let Some(symbol) = candidate {
            if !symbols.contains(&symbol) {
                symbols.push(symbol);
            }
        }

        if symbols.len() >= 12 {
            break;
        }
    }

    symbols
}

fn relative_display(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn build_codebase_entry(root: &Path, path: &Path, text: &str) -> CodebaseEntry {
    let rel = relative_display(path, root);
    let symbols = extract_symbols(path, text);
    let summary = first_meaningful_line(text);
    let excerpt: String = text.chars().take(1600).collect();
    let symbol_block = if symbols.is_empty() {
        "(no symbols extracted)".to_string()
    } else {
        symbols.join(", ")
    };

    CodebaseEntry {
        path: rel.clone(),
        kind: code_file_kind(path).to_string(),
        summary: summary.clone(),
        symbols,
        text: format!(
            "File: {rel}\nKind: {}\nRole: {summary}\nSymbols: {symbol_block}\n\nExcerpt:\n{excerpt}",
            code_file_kind(path)
        ),
    }
}

pub fn refresh_codebase_index() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let Some(root) = find_project_root() else {
        return Ok(0);
    };

    let mut files = Vec::new();
    collect_codebase_files(&root, &mut files)?;
    files.sort();

    let path = codebase_index_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;

    let mut indexed = 0usize;
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let entry = build_codebase_entry(&root, &file, &text);
        writeln!(output, "{}", serde_json::to_string(&entry)?)?;
        indexed += 1;
    }

    Ok(indexed)
}

pub fn ensure_codebase_index() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let path = codebase_index_path();
    let should_refresh = match fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|age| age > std::time::Duration::from_secs(600))
            .unwrap_or(true),
        Err(_) => true,
    };

    if should_refresh {
        refresh_codebase_index()
    } else {
        Ok(0)
    }
}

pub fn search_codebase(
    query: &str,
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let _ = ensure_codebase_index();

    let path = codebase_index_path();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let query_terms = tokenize(query);
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
        let Ok(entry) = serde_json::from_str::<CodebaseEntry>(&line) else {
            continue;
        };
        let score = score_entry(&entry.text, &query_terms);
        if score > 0.0 {
            let symbols = if entry.symbols.is_empty() {
                "(no symbols extracted)".to_string()
            } else {
                entry.symbols.join(", ")
            };
            let formatted = format!(
                "File: {}\nKind: {}\nRole: {}\nSymbols: {}",
                entry.path, entry.kind, entry.summary, symbols
            );
            scored.push((score, formatted));
        }
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored.into_iter().take(limit).map(|(_, t)| t).collect())
}

pub fn looks_like_code_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "code", "repo", "project", "architecture", "module", "function", "file", "cargo",
        "rust", "tauri", "src/", ".rs", "cargo.toml", "專案", "項目", "架構", "代碼",
        "程式碼", "模組", "函式", "檔案", "實作", "分析", "telegram", "memory",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || text.contains("::")
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

    #[test]
    fn detect_code_queries() {
        assert!(looks_like_code_query("幫我分析這個專案架構"));
        assert!(looks_like_code_query("memory.rs 的 append_context 在哪裡"));
        assert!(!looks_like_code_query("你好嗎"));
    }

    #[test]
    fn extract_rust_symbols_from_source() {
        let text = "pub struct SirinApp {}\npub async fn run_listener() {}\nfn helper() {}";
        let symbols = extract_symbols(Path::new("src/main.rs"), text);
        assert!(symbols.contains(&"SirinApp".to_string()));
        assert!(symbols.contains(&"run_listener".to_string()));
        assert!(symbols.contains(&"helper".to_string()));
    }
}
