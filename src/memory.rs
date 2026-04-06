//! Persistent memory for Sirin.
//!
//! Three layers:
//!
//! 1. **Full-text memory store** (`memory_store` / `memory_search`)
//!    SQLite FTS5 database at `data/memory/memories.db`.
//!    Searches use SQLite's built-in full-text index — 10-100× faster than the
//!    previous JSONL scan.  Existing JSONL data is migrated automatically on
//!    first startup.
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
use std::sync::{Mutex, OnceLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ── Storage usage ─────────────────────────────────────────────────────────────

/// Breakdown of Sirin's on-disk footprint across all managed data files.
#[derive(Debug, Clone, Default)]
pub struct StorageUsage {
    pub memory_db_bytes:    u64,   // SQLite FTS5 memories.db
    pub call_graph_bytes:   u64,   // call_graph.jsonl
    pub research_log_bytes: u64,   // tracking/research.jsonl
    pub task_log_bytes:     u64,   // tracking/task.jsonl
    pub context_bytes:      u64,   // context/*.jsonl (sum of all peers)
    pub total_bytes:        u64,
}

impl StorageUsage {
    /// Format a byte count as a readable string (B / KB / MB).
    pub fn fmt_bytes(b: u64) -> String {
        if b < 1_024 {
            format!("{b} B")
        } else if b < 1_024 * 1_024 {
            format!("{:.1} KB", b as f64 / 1_024.0)
        } else {
            format!("{:.2} MB", b as f64 / (1_024.0 * 1_024.0))
        }
    }
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn dir_size(path: &Path) -> u64 {
    fs::read_dir(path)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

/// Collect on-disk storage sizes for all Sirin data files.
pub fn storage_usage() -> StorageUsage {
    let base = if let Ok(local) = std::env::var("LOCALAPPDATA") {
        Path::new(&local).join("Sirin")
    } else {
        Path::new("data").to_path_buf()
    };

    let memory_db_bytes    = file_size(&base.join("memory").join("memories.db"));
    let call_graph_bytes   = file_size(&base.join("call_graph.jsonl"));
    let research_log_bytes = file_size(&base.join("tracking").join("research.jsonl"));
    let task_log_bytes     = file_size(&base.join("tracking").join("task.jsonl"));
    let context_bytes      = dir_size(&base.join("context"));
    let total_bytes        = memory_db_bytes + call_graph_bytes
                           + research_log_bytes + task_log_bytes + context_bytes;

    StorageUsage { memory_db_bytes, call_graph_bytes, research_log_bytes,
                   task_log_bytes, context_bytes, total_bytes }
}

// ── Memory store (SQLite FTS5 backend) ───────────────────────────────────────

fn memory_db_path() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return Path::new(&local_app_data)
            .join("Sirin")
            .join("memory")
            .join("memories.db");
    }
    Path::new("data").join("memory").join("memories.db")
}

/// Legacy JSONL path — used only for one-time migration on first startup.
fn memory_index_path() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return Path::new(&local_app_data)
            .join("Sirin")
            .join("memory")
            .join("index.jsonl");
    }
    Path::new("data").join("memory").join("index.jsonl")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryEntry {
    timestamp: String,
    source: String,
    text: String,
}

/// Return the process-wide SQLite connection (initialised once).
///
/// On first call, creates the FTS5 schema and migrates any existing JSONL data.
fn memory_db() -> &'static Mutex<rusqlite::Connection> {
    static DB: OnceLock<Mutex<rusqlite::Connection>> = OnceLock::new();
    DB.get_or_init(|| {
        let path = memory_db_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn = rusqlite::Connection::open(&path)
            .expect("Failed to open memory SQLite database");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts \
             USING fts5(text, source, timestamp, tokenize='unicode61');",
        )
        .expect("Failed to initialize memory FTS5 schema");

        // One-time migration from legacy JSONL on startup.
        migrate_jsonl_to_sqlite(&conn);

        Mutex::new(conn)
    })
}

/// Import any entries from the legacy JSONL file that don't yet exist in SQLite.
/// Safe to call repeatedly — skips migration when the FTS5 table is non-empty.
fn migrate_jsonl_to_sqlite(conn: &rusqlite::Connection) {
    let jsonl_path = memory_index_path();
    if !jsonl_path.exists() {
        return;
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories_fts", [], |r| r.get(0))
        .unwrap_or(0);
    if count > 0 {
        return; // Already migrated.
    }
    let file = match fs::File::open(&jsonl_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut stmt = match conn
        .prepare("INSERT INTO memories_fts(text, source, timestamp) VALUES (?1, ?2, ?3)")
    {
        Ok(s) => s,
        Err(_) => return,
    };
    let migrated = BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<MemoryEntry>(&l).ok())
        .filter_map(|e| {
            stmt.execute(rusqlite::params![e.text, e.source, e.timestamp]).ok()
        })
        .count();
    if migrated > 0 {
        eprintln!("[memory] migrated {migrated} JSONL entries → SQLite FTS5");
    }
}

/// Persist a text snippet to the memory store (SQLite FTS5).
pub fn memory_store(text: &str, source: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let timestamp = Utc::now().to_rfc3339();
    let conn = memory_db().lock().map_err(|e| format!("memory DB lock poisoned: {e}"))?;
    conn.execute(
        "INSERT INTO memories_fts(text, source, timestamp) VALUES (?1, ?2, ?3)",
        rusqlite::params![text, source, timestamp],
    )?;
    Ok(())
}

/// Full-text search the memory store using SQLite FTS5.
///
/// Results are ranked by FTS5 relevance (BM25) and capped at `limit`.
pub fn memory_search(
    query: &str,
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let safe_query = sanitize_fts5_query(query);
    let conn = memory_db().lock().map_err(|e| format!("memory DB lock poisoned: {e}"))?;
    let mut stmt = conn.prepare(
        "SELECT text FROM memories_fts \
         WHERE memories_fts MATCH ?1 \
         ORDER BY rank \
         LIMIT ?2",
    )?;
    let results: Vec<String> = stmt
        .query_map(rusqlite::params![safe_query, limit as i64], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(results)
}

/// Sanitize a user query string for use in an FTS5 MATCH expression.
///
/// Wraps each whitespace-separated token in double quotes so that special FTS5
/// syntax characters (parentheses, operators, etc.) can't cause parse errors.
fn sanitize_fts5_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|w| {
            let safe: String = w.chars().filter(|&c| c != '"').collect();
            format!("\"{safe}\"")
        })
        .collect();
    if tokens.is_empty() {
        return "\"\"".to_string();
    }
    tokens.join(" ")
}



// ── Text scoring utilities (used by codebase index search) ──────────────────

/// Tokenize text into lowercase words, splitting CJK characters individually.
///
/// Used by [`score_entry`] for codebase index search (TF-IDF).
/// The SQLite FTS5 memory backend handles its own tokenisation via the
/// `unicode61` tokenizer and does not use this function.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if is_cjk(ch) {
                if !word.is_empty() {
                    tokens.push(word.to_lowercase());
                    word.clear();
                }
                tokens.push(ch.to_string());
            } else {
                word.push(ch);
            }
        } else if !word.is_empty() {
            tokens.push(word.to_lowercase());
            word.clear();
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

/// Compute a simple TF score for `text` against a set of query terms.
///
/// Returns the sum of per-term term-frequency scores: each term's count in the
/// document divided by the total document length.  A score of 0.0 means none
/// of the query terms appear in the text.
///
/// Used by [`search_codebase`] for ranking local file entries by relevance.
/// Not used by the SQLite FTS5 memory backend which relies on BM25 ranking.
fn score_entry(text: &str, query_terms: &[String]) -> f64 {
    let doc_tokens = tokenize(text);
    let doc_len = doc_tokens.len().max(1) as f64;
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

fn is_summary_noise(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.starts_with("#![")
        || trimmed.starts_with("#[")
        || trimmed.starts_with("use ")
        || trimmed.starts_with("pub use ")
        || trimmed.starts_with("mod ")
        || trimmed.starts_with("pub mod ")
        || trimmed.starts_with("extern crate ")
}

fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !is_summary_noise(line))
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
    let canonical_root = fs::canonicalize(root).ok();
    let canonical_path = fs::canonicalize(path).ok();

    let display_path = canonical_path
        .as_deref()
        .zip(canonical_root.as_deref())
        .and_then(|(path, root)| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .or_else(|| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf());

    let display = display_path.to_string_lossy().replace('\\', "/");
    display.strip_prefix("//?/").unwrap_or(&display).to_string()
}

fn role_hint_for_path(rel: &str) -> Option<&'static str> {
    let rel = rel.replace('\\', "/");

    match rel.as_str() {
        "build.rs" => Some("Cargo 建置腳本，負責編譯期設定或資源處理。"),
        "Cargo.toml" => Some("Rust 專案清單，定義套件資訊、依賴與建置設定。"),
        "README.md" => Some("專案總覽與快速使用說明。"),
        "tauri.conf.json" => Some("桌面應用封裝與執行設定。"),
        "src/main.rs" => Some("應用程式入口，負責啟動 UI、agents、Telegram 與背景工作。"),
        "src/ui.rs" => Some("egui/eframe 桌面介面，負責聊天、任務板、日誌與 Telegram 授權 UI。"),
        "src/llm.rs" => Some("LLM 抽象層，負責連接 Ollama 與 OpenAI 相容後端（如 LM Studio）。"),
        "src/memory.rs" => Some("記憶與程式碼索引模組，負責本地檔案檢索、上下文與搜尋。"),
        "src/researcher.rs" => Some("調研任務管理與研究報告流程。"),
        "src/persona.rs" => Some("Persona 與行為規則設定。"),
        "src/skills.rs" => Some("技能與搜尋能力整合層。"),
        "src/log_buffer.rs" => Some("執行日誌緩衝與快照工具。"),
        "src/followup.rs" => Some("後續追蹤與待辦處理流程。"),
        "src/agents/chat_agent.rs" => Some("聊天 agent，負責整合本地檔案與程式碼內容來回答問題。"),
        "src/agents/planner_agent.rs" => Some("planner agent，先判斷使用者意圖與可能的步驟。"),
        "src/agents/router_agent.rs" => Some("router agent，決定要走 chat、research 或 follow-up 路線。"),
        "src/agents/research_agent.rs" => Some("research agent，負責調研、摘要與結果記錄。"),
        "src/agents/followup_agent.rs" => Some("follow-up agent，背景處理待辦與後續任務。"),
        "docs/ARCHITECTURE.md" => Some("架構設計說明文件。"),
        "docs/QUICKSTART.md" => Some("快速上手與執行說明。"),
        _ if rel.starts_with("src/adk/") => Some("ADK 執行框架元件，負責 context、tools、runner 與 agent runtime。"),
        _ if rel.starts_with("src/telegram/") => Some("Telegram 整合模組，處理 listener、回覆、語言與驗證。"),
        _ if rel.starts_with("docs/") => Some("專案文件，用來說明架構、路線圖或使用方式。"),
        _ => None,
    }
}

fn summarize_file_role(_path: &Path, rel: &str, text: &str) -> String {
    role_hint_for_path(rel)
        .map(str::to_string)
        .unwrap_or_else(|| first_meaningful_line(text))
}

fn build_codebase_entry(root: &Path, path: &Path, text: &str) -> CodebaseEntry {
    let rel = relative_display(path, root);
    let symbols = extract_symbols(path, text);
    let summary = summarize_file_role(path, &rel, text);
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

fn refresh_codebase_index_inner() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
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

/// Rebuild the codebase index *and* the call graph in one pass so they stay
/// in sync.  Called after every `file_write` and `file_patch`.
pub fn refresh_codebase_index() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let result = refresh_codebase_index_inner();
    // Invalidate and rebuild the call graph; failures are non-fatal.
    crate::code_graph::invalidate_cache();
    let _ = crate::code_graph::refresh_call_graph();
    result
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

fn project_file_priority(path: &str) -> i32 {
    let path = path.to_lowercase();
    let mut score = 0;

    if path == "cargo.toml" {
        score += 240;
    }
    if path == "readme.md" {
        score += 220;
    }
    if path == "tauri.conf.json" {
        score += 200;
    }
    if path == "docs/architecture.md" {
        score += 190;
    }
    if path == "src/main.rs" {
        score += 230;
    }
    if path == "src/ui.rs" {
        score += 225;
    }
    if path == "src/memory.rs" || path == "src/llm.rs" {
        score += 210;
    }
    if path.starts_with("src/agents/") {
        score += 180;
    }
    if path.starts_with("src/adk/") {
        score += 170;
    }
    if path.starts_with("src/telegram/") {
        score += 160;
    }
    if path.starts_with("src/") {
        score += 140;
    }
    if path.starts_with("app/") {
        score += 120;
    }
    if path.starts_with("docs/") {
        score += 110;
    }
    if path.starts_with("config/") {
        score += 80;
    }
    if path.starts_with("tests/") {
        score += 40;
    }
    if path.starts_with('.') {
        score -= 160;
    }
    if path.starts_with(".claude/") || path.starts_with(".github/") {
        score -= 180;
    }

    score
}

pub fn list_project_files(
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(root) = find_project_root() else {
        return Ok(Vec::new());
    };

    let mut files = Vec::new();
    collect_codebase_files(&root, &mut files)?;

    let mut rel_files: Vec<String> = files
        .into_iter()
        .map(|path| relative_display(&path, &root))
        .collect();

    rel_files.sort_by(|a, b| {
        project_file_priority(b)
            .cmp(&project_file_priority(a))
            .then_with(|| a.cmp(b))
    });
    rel_files.dedup();

    Ok(rel_files.into_iter().take(limit).collect())
}

fn normalize_path_hint(path_hint: &str) -> String {
    path_hint
        .trim()
        .trim_matches(|c| matches!(c, '`' | '"' | '\''))
        .trim_matches(|c: char| matches!(c, ',' | '，' | '。' | '?' | '？' | ':' | '：' | '(' | ')'))
        .replace('\\', "/")
}

fn resolve_project_file_path(root: &Path, path_hint: &str) -> Option<PathBuf> {
    let normalized = normalize_path_hint(path_hint);
    if normalized.is_empty() {
        return None;
    }

    let root_canonical = fs::canonicalize(root).ok().unwrap_or_else(|| root.to_path_buf());
    let direct = PathBuf::from(&normalized);
    let candidate = if direct.is_absolute() {
        direct
    } else {
        root.join(&normalized)
    };

    if candidate.is_file() {
        let canonical = fs::canonicalize(&candidate).ok().unwrap_or(candidate.clone());
        if canonical.starts_with(&root_canonical) {
            return Some(candidate);
        }
    }

    let mut files = Vec::new();
    collect_codebase_files(root, &mut files).ok()?;
    let normalized_lower = normalized.to_lowercase();

    files.into_iter().find(|path| {
        let rel = relative_display(path, root).to_lowercase();
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_lowercase();
        rel == normalized_lower || rel.ends_with(&normalized_lower) || name == normalized_lower
    })
}

pub fn inspect_project_file(
    path_hint: &str,
    max_chars: usize,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    inspect_project_file_range(path_hint, None, None, max_chars)
}

/// Like [`inspect_project_file`] but returns only lines `start_line..=end_line`
/// (1-based, inclusive on both ends).  When both are `None` the full file is
/// returned up to `max_chars`.  The output includes line numbers so the agent
/// can reference exact positions for `file_patch`.
pub fn inspect_project_file_range(
    path_hint: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_chars: usize,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let Some(root) = find_project_root() else {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "project root not found").into());
    };

    let path = resolve_project_file_path(&root, path_hint).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("could not resolve local project file: {path_hint}"),
        )
    })?;

    let text = fs::read_to_string(&path)?;
    let rel = relative_display(&path, &root);
    let summary = summarize_file_role(&path, &rel, &text);
    let total_lines = text.lines().count();

    let excerpt = if start_line.is_some() || end_line.is_some() {
        // Line-range mode: return numbered lines in [start..=end].
        let start = start_line.unwrap_or(1).max(1);
        let end = end_line.unwrap_or(total_lines).min(total_lines);
        let cap = max_chars.clamp(400, 16000);
        let mut out = String::new();
        let mut chars_used = 0usize;
        for (i, line) in text.lines().enumerate() {
            let lineno = i + 1;
            if lineno < start { continue; }
            if lineno > end { break; }
            let entry = format!("{lineno:>5} | {line}\n");
            if chars_used + entry.len() > cap { break; }
            out.push_str(&entry);
            chars_used += entry.len();
        }
        out
    } else {
        // Full-file mode (char-truncated, same as before).
        text.chars().take(max_chars.clamp(400, 4000)).collect()
    };

    let range_note = match (start_line, end_line) {
        (Some(s), Some(e)) => format!(" [lines {s}–{e} of {total_lines}]"),
        (Some(s), None) => format!(" [lines {s}–{total_lines} of {total_lines}]"),
        (None, Some(e)) => format!(" [lines 1–{e} of {total_lines}]"),
        (None, None) => format!(" [{total_lines} lines total]"),
    };

    Ok(format!(
        "File: {rel}{range_note}\nKind: {}\nRole: {summary}\n\nExcerpt:\n{excerpt}",
        code_file_kind(&path)
    ))
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
    let mut scored: Vec<(f64, String, Vec<String>)> = Vec::new();

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
            scored.push((score, formatted, entry.symbols));
        }
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let results: Vec<String> = scored
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(rank, (_, mut text, symbols))| {
            // For the top result, append 1-hop call graph context for each
            // extracted symbol so the LLM immediately sees callers / callees.
            if rank == 0 && !symbols.is_empty() {
                let mut graph_lines: Vec<String> = Vec::new();
                for sym in symbols.iter().take(4) {
                    if let Ok(cg) = crate::code_graph::query_call_graph(sym, 1) {
                        let has_info = cg.defined_in.is_some()
                            || !cg.callers.is_empty()
                            || !cg.callees.is_empty();
                        if has_info {
                            let mut parts = Vec::new();
                            if let Some(loc) = &cg.defined_in {
                                parts.push(format!("defined at {loc}"));
                            }
                            if !cg.callers.is_empty() {
                                parts.push(format!("called by: {}", cg.callers.join(", ")));
                            }
                            if !cg.callees.is_empty() {
                                parts.push(format!("calls: {}", cg.callees.join(", ")));
                            }
                            graph_lines.push(format!("  {sym}: {}", parts.join(" | ")));
                        }
                    }
                }
                if !graph_lines.is_empty() {
                    text.push_str("\nCall graph:\n");
                    text.push_str(&graph_lines.join("\n"));
                }
            }
            text
        })
        .collect();

    Ok(results)
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
    fn can_inspect_local_project_file() {
        let excerpt = inspect_project_file("src/main.rs", 600)
            .expect("should read a real project file from the local workspace");
        assert!(excerpt.contains("File: src/main.rs"));
        assert!(excerpt.contains("Role: 應用程式入口"));
        assert!(excerpt.contains("Excerpt:"));
    }

    #[test]
    fn skips_rust_attributes_when_deriving_summary() {
        let summary = first_meaningful_line("#![cfg_attr(not(debug_assertions), windows_subsystem = \"windows\")]\n//! App bootstrap\nmod ui;");
        assert_eq!(summary, "App bootstrap");
    }

    #[test]
    fn known_files_use_human_friendly_role_hints() {
        assert_eq!(
            summarize_file_role(Path::new("src/main.rs"), "src/main.rs", "#![cfg_attr(...)]\nfn main() {}"),
            "應用程式入口，負責啟動 UI、agents、Telegram 與背景工作。"
        );
    }

    #[test]
    fn prioritizes_core_project_files_over_hidden_command_docs() {
        assert!(project_file_priority("src/main.rs") > project_file_priority(".claude/commands/build-check.md"));
        assert!(project_file_priority("Cargo.toml") > project_file_priority(".claude/commands/build-check.md"));
    }

    #[test]
    fn extract_rust_symbols_from_source() {
        let text = "pub struct SirinApp {}\npub async fn run_listener() {}\nfn helper() {}";
        let symbols = extract_symbols(Path::new("src/main.rs"), text);
        assert!(symbols.contains(&"SirinApp".to_string()));
        assert!(symbols.contains(&"run_listener".to_string()));
        assert!(symbols.contains(&"helper".to_string()));
    }

    // ── inspect_project_file_range tests ─────────────────────────────────────

    #[test]
    fn range_read_returns_numbered_lines() {
        let result = inspect_project_file_range("src/main.rs", Some(1), Some(3), 4000);
        let content = result.expect("should find src/main.rs");
        // Output must include line numbers in "  N | ..." format.
        assert!(content.contains("    1 |"), "line 1 should be numbered: {content}");
        assert!(content.contains("    2 |") || content.contains("    3 |"), "line 2 or 3 should appear");
        // Lines after the range should not be present (file has > 3 lines).
        let excerpt_start = content.find("Excerpt:").expect("should have Excerpt section");
        let excerpt = &content[excerpt_start..];
        assert!(!excerpt.contains("    4 |"), "line 4 should be outside range");
    }

    #[test]
    fn range_read_full_file_no_line_numbers() {
        let result = inspect_project_file_range("src/main.rs", None, None, 4000);
        let content = result.expect("should find src/main.rs");
        // Full-file mode does not add line number prefixes.
        assert!(!content.contains("    1 |"), "full-file mode should not number lines");
        assert!(content.contains("Excerpt:"), "should have Excerpt section");
    }

    #[test]
    fn range_read_start_beyond_eof_returns_empty_excerpt() {
        let result = inspect_project_file_range("src/main.rs", Some(99999), None, 4000);
        let content = result.expect("should find src/main.rs even with out-of-range start");
        let excerpt_start = content.find("Excerpt:").expect("should have Excerpt section");
        let excerpt = &content[excerpt_start + "Excerpt:".len()..].trim().to_string();
        // Excerpt should be empty or very short (no matching lines).
        assert!(excerpt.len() < 20, "excerpt should be empty for out-of-range start: '{excerpt}'");
    }

    #[test]
    fn range_note_included_in_output() {
        let result = inspect_project_file_range("src/main.rs", Some(1), Some(5), 4000);
        let content = result.expect("should find src/main.rs");
        // The header line should mention the range.
        assert!(content.contains("lines 1"), "range note should appear in header: {content}");
    }

    // ── collect_codebase_files ────────────────────────────────────────────────

    #[test]
    fn collect_skips_excluded_dirs() {
        use std::io;

        let tmp = std::env::temp_dir().join(format!("sirin_collect_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join("target/debug")).unwrap();
        fs::create_dir_all(tmp.join(".git/objects")).unwrap();
        fs::create_dir_all(tmp.join("node_modules/pkg")).unwrap();

        fs::write(tmp.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.join("target/debug/artifact.rs"), "// should skip").unwrap();
        fs::write(tmp.join(".git/objects/pack"), "binary").unwrap();
        fs::write(tmp.join("node_modules/pkg/index.js"), "module.exports={}").unwrap();

        let mut found: Vec<std::path::PathBuf> = Vec::new();
        collect_codebase_files(&tmp, &mut found).unwrap();

        let names: Vec<String> = found.iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();

        assert!(names.iter().any(|n| n.ends_with("src/main.rs")), "should include src/main.rs");
        assert!(!names.iter().any(|n| n.contains("/target/")), "should skip target/");
        assert!(!names.iter().any(|n| n.contains("/.git/")), "should skip .git/");
        assert!(!names.iter().any(|n| n.contains("/node_modules/")), "should skip node_modules/");

        let _ = fs::remove_dir_all(&tmp);
        let _: io::Result<()> = Ok(());
    }

    // ── is_codebase_candidate ─────────────────────────────────────────────────

    #[test]
    fn candidate_accepts_source_extensions() {
        let tmp = std::env::temp_dir().join(format!("sirin_cand_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        for ext in ["main.rs", "Cargo.toml", "README.md", "config.yaml", "app.ts", "page.tsx"] {
            let p = tmp.join(ext);
            fs::write(&p, "content").unwrap();
            let meta = fs::metadata(&p).unwrap();
            assert!(is_codebase_candidate(&p, &meta), "should accept .{}", ext);
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn candidate_rejects_unknown_extensions() {
        let tmp = std::env::temp_dir().join(format!("sirin_rej_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        for name in ["binary.exe", "archive.zip", "image.png", "data.bin"] {
            let p = tmp.join(name);
            fs::write(&p, "content").unwrap();
            let meta = fs::metadata(&p).unwrap();
            assert!(!is_codebase_candidate(&p, &meta), "should reject {name}");
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── extract_symbols ───────────────────────────────────────────────────────

    #[test]
    fn extract_symbols_handles_async_and_pub() {
        let src = "pub async fn run_listener() {}\nasync fn helper_task() {}\npub struct Config {}";
        let syms = extract_symbols(std::path::Path::new("src/foo.rs"), src);
        assert!(syms.contains(&"run_listener".to_string()), "should extract pub async fn");
        assert!(syms.contains(&"helper_task".to_string()), "should extract async fn");
        assert!(syms.contains(&"Config".to_string()), "should extract pub struct");
    }

    #[test]
    fn extract_symbols_caps_at_twelve() {
        // Generate 20 distinct functions.
        let src: String = (0..20).map(|i| format!("fn func_{i}() {{}}\n")).collect();
        let syms = extract_symbols(std::path::Path::new("src/foo.rs"), &src);
        assert!(syms.len() <= 12, "should cap at 12 symbols, got {}", syms.len());
    }

    // ── refresh_codebase_index ────────────────────────────────────────────────

    #[test]
    fn refresh_returns_nonzero_for_real_project() {
        // run against the actual Sirin workspace (Cargo.toml is present in cwd)
        let count = refresh_codebase_index();
        assert!(count.is_ok(), "refresh should succeed: {:?}", count.err());
        assert!(count.unwrap() > 0, "should have indexed at least one file");
    }

    #[test]
    fn search_codebase_finds_relevant_results() {
        // Ensure index exists first.
        let _ = refresh_codebase_index();
        let results = search_codebase("planner agent intent", 5);
        assert!(results.is_ok(), "search should succeed");
        let results = results.unwrap();
        // The planner_agent module must surface in results.
        assert!(
            results.iter().any(|r| r.to_lowercase().contains("planner")),
            "planner should appear in results: {:?}", results
        );
    }
}
