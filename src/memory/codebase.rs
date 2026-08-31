//! Project codebase index — scans the local repo, extracts file summaries
//! and symbol lists, and supports TF-scored keyword search.
//!
//! Storage: `{app_data}/memory/codebase_index.jsonl` (one JSON entry per file).
//! Refreshes on demand via [`refresh_codebase_index`], or via
//! [`ensure_codebase_index`] if the file is stale (> 10 min old).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

// ── Text scoring utilities ────────────────────────────────────────────────────

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

// ── Index file layout ─────────────────────────────────────────────────────────

fn codebase_index_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("memory")
        .join("codebase_index.jsonl")
}

fn codebase_index_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn codebase_index_is_stale(path: &Path) -> bool {
    match fs::metadata(path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|age| age > std::time::Duration::from_secs(600))
            .unwrap_or(true),
        Err(_) => true,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodebaseEntry {
    path: String,
    kind: String,
    summary: String,
    symbols: Vec<String>,
    text: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    modified_unix_nanos: u64,
}

fn read_codebase_entries(
    path: &Path,
) -> Result<Vec<CodebaseEntry>, Box<dyn std::error::Error + Send + Sync>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path)?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<CodebaseEntry>(&line).ok())
        .collect())
}

fn persist_codebase_entries(
    path: &Path,
    entries: &[CodebaseEntry],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    crate::jsonl_log::atomic_write_jsonl(path, entries)?;
    Ok(())
}

// ── Project traversal ─────────────────────────────────────────────────────────

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
    match path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
    {
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
        path.extension()
            .and_then(|v| v.to_str())
            .unwrap_or_default(),
        "rs" | "toml" | "md" | "yaml" | "yml" | "ts" | "tsx" | "js" | "jsx" | "json"
    )
}

fn collect_codebase_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            let name = path
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or_default();
            if matches!(
                name,
                ".git" | "target" | "node_modules" | ".next" | "dist" | "build"
            ) {
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

// ── Entry construction (summary, symbols, excerpt) ────────────────────────────

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
    let ext = path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default();
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
            "ts" | "tsx" | "js" | "jsx" => {
                capture_symbol_after_prefix(trimmed, "export async function ")
                    .or_else(|| capture_symbol_after_prefix(trimmed, "export function "))
                    .or_else(|| capture_symbol_after_prefix(trimmed, "function "))
                    .or_else(|| capture_symbol_after_prefix(trimmed, "export const "))
                    .or_else(|| capture_symbol_after_prefix(trimmed, "const "))
                    .or_else(|| capture_symbol_after_prefix(trimmed, "export default function "))
            }
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
        "web/index.html" => Some("Plain HTML/Alpine.js 網頁介面，由 mcp_server 在 :7700/ui/ serve；單檔、無 build。"),
        "web/app.js"     => Some("網頁 UI 的 sirin() Alpine factory：state、fetch /api/snapshot、WebSocket 連線、widget 動作。"),
        "web/style.css"  => Some("網頁 UI 樣式：CSS variable design token + widget grid + KPI 卡片。"),
        "src/llm.rs" => Some("LLM 抽象層，負責連接 Ollama 與 OpenAI 相容後端（如 LM Studio）。"),
        "src/memory.rs" => Some("記憶與程式碼索引模組，負責本地檔案檢索、上下文與搜尋。"),
        "src/researcher.rs" => Some("調研任務管理與研究報告流程。"),
        "src/persona.rs" => Some("Persona 與行為規則設定。"),
        "src/skills.rs" => Some("技能與搜尋能力整合層。"),
        "src/log_buffer.rs" => Some("執行日誌緩衝與快照工具。"),
        "src/followup.rs" => Some("後續追蹤與待辦處理流程。"),
        "src/agents/chat_agent.rs" => Some("聊天 agent，負責整合本地檔案與程式碼內容來回答問題。"),
        "src/agents/planner_agent.rs" => Some("planner agent，先判斷使用者意圖與可能的步驟。"),
        "src/agents/router_agent.rs" => {
            Some("router agent，決定要走 chat、research 或 follow-up 路線。")
        }
        "src/agents/research_agent.rs" => Some("research agent，負責調研、摘要與結果記錄。"),
        "docs/ARCHITECTURE.md" => Some("架構設計說明文件。"),
        "docs/QUICKSTART.md" => Some("快速上手與執行說明。"),
        _ if rel.starts_with("src/adk/") => {
            Some("ADK 執行框架元件，負責 context、tools、runner 與 agent runtime。")
        }
        _ if rel.starts_with("src/telegram/") => {
            Some("Telegram 整合模組，處理 listener、回覆、語言與驗證。")
        }
        _ if rel.starts_with("docs/") => Some("專案文件，用來說明架構、路線圖或使用方式。"),
        _ => None,
    }
}

fn summarize_file_role(_path: &Path, rel: &str, text: &str) -> String {
    role_hint_for_path(rel)
        .map(str::to_string)
        .unwrap_or_else(|| first_meaningful_line(text))
}

fn metadata_modified_unix_nanos(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
        .unwrap_or(0)
}

fn build_codebase_entry(
    root: &Path,
    path: &Path,
    text: &str,
    metadata: &fs::Metadata,
) -> CodebaseEntry {
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
        size_bytes: metadata.len(),
        modified_unix_nanos: metadata_modified_unix_nanos(metadata),
    }
}

fn update_codebase_entries_for_file(
    root: &Path,
    path: &Path,
    entries: &mut Vec<CodebaseEntry>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rel = relative_display(path, root);
    entries.retain(|entry| entry.path != rel);

    let metadata = fs::metadata(path)?;
    if is_codebase_candidate(path, &metadata) {
        let text = fs::read_to_string(path)?;
        if !text.trim().is_empty() {
            entries.push(build_codebase_entry(root, path, &text, &metadata));
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

#[derive(Debug, Default)]
struct CodebaseDelta {
    changed_paths: Vec<PathBuf>,
    removed_paths: Vec<String>,
}

fn detect_codebase_delta(
    root: &Path,
    files: &[PathBuf],
    entries: &[CodebaseEntry],
) -> Result<CodebaseDelta, Box<dyn std::error::Error + Send + Sync>> {
    let indexed: HashMap<&str, (u64, u64)> = entries
        .iter()
        .map(|entry| {
            (
                entry.path.as_str(),
                (entry.size_bytes, entry.modified_unix_nanos),
            )
        })
        .collect();
    let mut current_paths = HashSet::new();
    let mut changed_paths = Vec::new();

    for path in files {
        let metadata = fs::metadata(path)?;
        let rel = relative_display(path, root);
        current_paths.insert(rel.clone());
        let fingerprint = (metadata.len(), metadata_modified_unix_nanos(&metadata));
        if indexed.get(rel.as_str()).copied() != Some(fingerprint) {
            changed_paths.push(path.clone());
        }
    }

    let removed_paths = entries
        .iter()
        .filter(|entry| !current_paths.contains(&entry.path))
        .map(|entry| entry.path.clone())
        .collect();

    Ok(CodebaseDelta {
        changed_paths,
        removed_paths,
    })
}

// ── Public API: refresh / ensure / list / inspect / search ────────────────────

fn build_codebase_entries(
    root: &Path,
) -> Result<Vec<CodebaseEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let mut files = Vec::new();
    collect_codebase_files(root, &mut files)?;
    files.sort();

    let mut entries = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let Ok(metadata) = fs::metadata(&file) else {
            continue;
        };
        entries.push(build_codebase_entry(root, &file, &text, &metadata));
    }

    Ok(entries)
}

fn refresh_codebase_index_inner_at(
    root: &Path,
    index_path: &Path,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let entries = build_codebase_entries(root)?;

    let _guard = codebase_index_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    persist_codebase_entries(index_path, &entries)?;
    Ok(entries.len())
}

fn refresh_codebase_index_inner() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let Some(root) = find_project_root() else {
        return Ok(0);
    };
    refresh_codebase_index_inner_at(&root, &codebase_index_path())
}

fn refresh_stale_codebase_index() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let root = find_project_root().ok_or("Cannot determine project root")?;
    let canonical_root = fs::canonicalize(&root)?;
    let mut files = Vec::new();
    collect_codebase_files(&canonical_root, &mut files)?;
    files.sort();

    let (entry_count, delta) = {
        let _guard = codebase_index_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let index_path = codebase_index_path();
        let mut entries = read_codebase_entries(&index_path)?;
        let delta = detect_codebase_delta(&canonical_root, &files, &entries)?;
        let removed: HashSet<&str> = delta.removed_paths.iter().map(String::as_str).collect();
        entries.retain(|entry| !removed.contains(entry.path.as_str()));
        for path in &delta.changed_paths {
            update_codebase_entries_for_file(&canonical_root, path, &mut entries)?;
        }
        persist_codebase_entries(&index_path, &entries)?;
        (entries.len(), delta)
    };

    let changed_rust: Vec<PathBuf> = delta
        .changed_paths
        .into_iter()
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
        .collect();
    let removed_rust: Vec<String> = delta
        .removed_paths
        .into_iter()
        .filter(|path| Path::new(path).extension().and_then(|ext| ext.to_str()) == Some("rs"))
        .collect();
    if let Err(error) = crate::code_graph::refresh_call_graph_files(&changed_rust, &removed_rust) {
        eprintln!("[memory] external-change call graph refresh failed: {error}");
    }

    Ok(entry_count)
}

/// Rebuild the complete codebase index and call graph so they stay in sync.
/// Normal coding-tool writes use [`refresh_codebase_file`]; this full path is
/// reserved for initial creation and explicit rebuilds.
pub fn refresh_codebase_index() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let result = refresh_codebase_index_inner();
    // Invalidate and rebuild the call graph; failures are non-fatal.
    crate::code_graph::invalidate_cache();
    let _ = crate::code_graph::refresh_call_graph();
    result
}

/// Incrementally refresh the codebase and call-graph entries for one file.
///
/// A missing index still triggers one full build. Once the index exists,
/// coding-agent writes update only the touched file so a small patch does not
/// synchronously rescan the entire repository.
pub fn refresh_codebase_file(
    path: &Path,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let root = find_project_root().ok_or("Cannot determine project root")?;
    let canonical_root = fs::canonicalize(&root)?;
    let canonical_path = fs::canonicalize(path)?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(format!(
            "codebase refresh path '{}' is outside project root '{}'",
            canonical_path.display(),
            canonical_root.display()
        )
        .into());
    }

    let index_path = codebase_index_path();
    if !index_path.exists() {
        return refresh_codebase_index();
    }
    if codebase_index_is_stale(&index_path) {
        return refresh_stale_codebase_index();
    }

    let entry_count = {
        let _guard = codebase_index_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut entries = read_codebase_entries(&index_path)?;
        update_codebase_entries_for_file(&canonical_root, &canonical_path, &mut entries)?;
        persist_codebase_entries(&index_path, &entries)?;
        entries.len()
    };

    if let Err(error) = crate::code_graph::refresh_call_graph_file(&canonical_path) {
        eprintln!("[memory] incremental call graph refresh failed: {error}");
    }
    Ok(entry_count)
}

pub fn ensure_codebase_index() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let path = codebase_index_path();
    if !path.exists() {
        refresh_codebase_index()
    } else if codebase_index_is_stale(&path) {
        refresh_stale_codebase_index()
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

// ── Path resolution ───────────────────────────────────────────────────────────

fn normalize_path_hint(path_hint: &str) -> String {
    path_hint
        .trim()
        .trim_matches(|c| matches!(c, '`' | '"' | '\''))
        .trim_matches(|c: char| {
            matches!(c, ',' | '，' | '。' | '?' | '？' | ':' | '：' | '(' | ')')
        })
        .replace('\\', "/")
}

fn resolve_project_file_path(root: &Path, path_hint: &str) -> Option<PathBuf> {
    let normalized = normalize_path_hint(path_hint);
    if normalized.is_empty() {
        return None;
    }

    let root_canonical = fs::canonicalize(root)
        .ok()
        .unwrap_or_else(|| root.to_path_buf());
    let direct = PathBuf::from(&normalized);
    let candidate = if direct.is_absolute() {
        direct
    } else {
        root.join(&normalized)
    };
    let rust_mod_candidate = normalized
        .strip_suffix(".rs")
        .map(|stem| root.join(stem).join("mod.rs"));

    if candidate.is_file() {
        let canonical = fs::canonicalize(&candidate)
            .ok()
            .unwrap_or(candidate.clone());
        if canonical.starts_with(&root_canonical) {
            return Some(candidate);
        }
    }

    if let Some(mod_candidate) = rust_mod_candidate {
        if mod_candidate.is_file() {
            let canonical = fs::canonicalize(&mod_candidate)
                .ok()
                .unwrap_or(mod_candidate.clone());
            if canonical.starts_with(&root_canonical) {
                return Some(mod_candidate);
            }
        }
    }

    let mut files = Vec::new();
    collect_codebase_files(root, &mut files).ok()?;
    let normalized_lower = normalized.to_lowercase();
    let rust_mod_suffix = normalized_lower
        .strip_suffix(".rs")
        .map(|stem| format!("{stem}/mod.rs"));

    files.into_iter().find(|path| {
        let rel = relative_display(path, root).to_lowercase();
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_lowercase();
        rel == normalized_lower
            || rel.ends_with(&normalized_lower)
            || name == normalized_lower
            || rust_mod_suffix
                .as_ref()
                .map(|suffix| rel == *suffix || rel.ends_with(suffix))
                .unwrap_or(false)
    })
}

#[allow(dead_code)]
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
        return Err(
            std::io::Error::new(std::io::ErrorKind::NotFound, "project root not found").into(),
        );
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
            if lineno < start {
                continue;
            }
            if lineno > end {
                break;
            }
            let entry = format!("{lineno:>5} | {line}\n");
            if chars_used + entry.len() > cap {
                break;
            }
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
    search_codebase_index(&path, query, limit)
}

fn search_codebase_index(
    path: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let query_terms = tokenize(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }

    let _guard = codebase_index_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
        "code",
        "repo",
        "project",
        "architecture",
        "module",
        "function",
        "file",
        "cargo",
        "rust",
        "tauri",
        "src/",
        ".rs",
        "cargo.toml",
        "專案",
        "項目",
        "架構",
        "代碼",
        "程式碼",
        "模組",
        "函式",
        "檔案",
        "實作",
        "分析",
        "telegram",
        "memory",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || text.contains("::")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture_codebase_project(label: &str) -> (PathBuf, PathBuf) {
        static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sirin_codebase_{label}_{}_{}",
            std::process::id(),
            sequence
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("src")).expect("create fixture src");
        std::fs::create_dir_all(root.join("docs")).expect("create fixture docs");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .expect("write fixture manifest");
        std::fs::write(root.join("src/main.rs"), "pub fn bootstrap_runtime() {}\n")
            .expect("write fixture source");
        std::fs::write(
            root.join("docs/planner.md"),
            "# Planner agent intent routing\nPlanner agent classifies intent before routing.\n",
        )
        .expect("write fixture documentation");
        let index_path = root.join("data/codebase_index.jsonl");
        (root, index_path)
    }

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
            summarize_file_role(
                Path::new("src/main.rs"),
                "src/main.rs",
                "#![cfg_attr(...)]\nfn main() {}"
            ),
            "應用程式入口，負責啟動 UI、agents、Telegram 與背景工作。"
        );
    }

    #[test]
    fn prioritizes_core_project_files_over_hidden_command_docs() {
        assert!(
            project_file_priority("src/main.rs")
                > project_file_priority(".claude/commands/build-check.md")
        );
        assert!(
            project_file_priority("Cargo.toml")
                > project_file_priority(".claude/commands/build-check.md")
        );
    }

    #[test]
    fn extract_rust_symbols_from_source() {
        let text = "pub struct SirinApp {}\npub async fn run_listener() {}\nfn helper() {}";
        let symbols = extract_symbols(Path::new("src/main.rs"), text);
        assert!(symbols.contains(&"SirinApp".to_string()));
        assert!(symbols.contains(&"run_listener".to_string()));
        assert!(symbols.contains(&"helper".to_string()));
    }

    #[test]
    fn range_read_returns_numbered_lines() {
        let result = inspect_project_file_range("src/main.rs", Some(1), Some(3), 4000);
        let content = result.expect("should find src/main.rs");
        assert!(
            content.contains("    1 |"),
            "line 1 should be numbered: {content}"
        );
        assert!(
            content.contains("    2 |") || content.contains("    3 |"),
            "line 2 or 3 should appear"
        );
        let excerpt_start = content
            .find("Excerpt:")
            .expect("should have Excerpt section");
        let excerpt = &content[excerpt_start..];
        assert!(
            !excerpt.contains("    4 |"),
            "line 4 should be outside range"
        );
    }

    #[test]
    fn range_read_full_file_no_line_numbers() {
        let result = inspect_project_file_range("src/main.rs", None, None, 4000);
        let content = result.expect("should find src/main.rs");
        assert!(
            !content.contains("    1 |"),
            "full-file mode should not number lines"
        );
        assert!(content.contains("Excerpt:"), "should have Excerpt section");
    }

    #[test]
    fn range_read_start_beyond_eof_returns_empty_excerpt() {
        let result = inspect_project_file_range("src/main.rs", Some(99999), None, 4000);
        let content = result.expect("should find src/main.rs even with out-of-range start");
        let excerpt_start = content
            .find("Excerpt:")
            .expect("should have Excerpt section");
        let excerpt = &content[excerpt_start + "Excerpt:".len()..]
            .trim()
            .to_string();
        assert!(
            excerpt.len() < 20,
            "excerpt should be empty for out-of-range start: '{excerpt}'"
        );
    }

    #[test]
    fn range_note_included_in_output() {
        let result = inspect_project_file_range("src/main.rs", Some(1), Some(5), 4000);
        let content = result.expect("should find src/main.rs");
        assert!(
            content.contains("lines 1"),
            "range note should appear in header: {content}"
        );
    }

    #[test]
    fn range_read_resolves_rust_module_path_to_mod_rs() {
        let result = inspect_project_file_range("src/telegram.rs", Some(1), Some(5), 4000);
        let content = result.expect("should resolve Rust module path to src/telegram/mod.rs");
        assert!(
            content.contains("File: src/telegram/mod.rs"),
            "expected module path resolution, got: {content}"
        );
    }

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

        let names: Vec<String> = found
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();

        assert!(
            names.iter().any(|n| n.ends_with("src/main.rs")),
            "should include src/main.rs"
        );
        assert!(
            !names.iter().any(|n| n.contains("/target/")),
            "should skip target/"
        );
        assert!(
            !names.iter().any(|n| n.contains("/.git/")),
            "should skip .git/"
        );
        assert!(
            !names.iter().any(|n| n.contains("/node_modules/")),
            "should skip node_modules/"
        );

        let _ = fs::remove_dir_all(&tmp);
        let _: io::Result<()> = Ok(());
    }

    #[test]
    fn candidate_accepts_source_extensions() {
        let tmp = std::env::temp_dir().join(format!("sirin_cand_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        for ext in [
            "main.rs",
            "Cargo.toml",
            "README.md",
            "config.yaml",
            "app.ts",
            "page.tsx",
        ] {
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

    #[test]
    fn extract_symbols_handles_async_and_pub() {
        let src = "pub async fn run_listener() {}\nasync fn helper_task() {}\npub struct Config {}";
        let syms = extract_symbols(std::path::Path::new("src/foo.rs"), src);
        assert!(
            syms.contains(&"run_listener".to_string()),
            "should extract pub async fn"
        );
        assert!(
            syms.contains(&"helper_task".to_string()),
            "should extract async fn"
        );
        assert!(
            syms.contains(&"Config".to_string()),
            "should extract pub struct"
        );
    }

    #[test]
    fn extract_symbols_caps_at_twelve() {
        let src: String = (0..20).map(|i| format!("fn func_{i}() {{}}\n")).collect();
        let syms = extract_symbols(std::path::Path::new("src/foo.rs"), &src);
        assert!(
            syms.len() <= 12,
            "should cap at 12 symbols, got {}",
            syms.len()
        );
    }

    #[test]
    fn incremental_refresh_updates_one_entry_without_dropping_others() {
        let root =
            std::env::temp_dir().join(format!("sirin_codebase_incremental_{}", std::process::id()));
        let src_dir = root.join("src");
        let changed = src_dir.join("changed.rs");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&src_dir).expect("create temp source tree");
        std::fs::write(&changed, "pub fn old_symbol() {}\n").expect("write source");

        let mut entries = vec![CodebaseEntry {
            path: "docs/untouched.md".to_string(),
            kind: "documentation".to_string(),
            summary: "untouched".to_string(),
            symbols: Vec::new(),
            text: "untouched".to_string(),
            size_bytes: 0,
            modified_unix_nanos: 0,
        }];
        update_codebase_entries_for_file(&root, &changed, &mut entries)
            .expect("initial incremental refresh");
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|entry| entry.symbols.iter().any(|symbol| symbol == "old_symbol")));

        std::fs::write(&changed, "pub fn new_symbol() {}\n").expect("rewrite source");
        update_codebase_entries_for_file(&root, &changed, &mut entries)
            .expect("replacement incremental refresh");
        assert_eq!(entries.len(), 2, "changed file entry must be replaced");
        assert!(entries
            .iter()
            .any(|entry| entry.path == "docs/untouched.md"));
        assert!(entries
            .iter()
            .any(|entry| entry.symbols.iter().any(|symbol| symbol == "new_symbol")));
        assert!(!entries
            .iter()
            .any(|entry| entry.symbols.iter().any(|symbol| symbol == "old_symbol")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stale_scan_detects_changed_new_and_removed_files() {
        let root =
            std::env::temp_dir().join(format!("sirin_codebase_delta_{}", std::process::id()));
        let src_dir = root.join("src");
        let changed = src_dir.join("changed.rs");
        let removed = src_dir.join("removed.rs");
        let added = src_dir.join("added.rs");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&src_dir).expect("create temp source tree");
        std::fs::write(&changed, "fn old() {}\n").expect("write changed source");
        std::fs::write(&removed, "fn removed() {}\n").expect("write removed source");

        let mut entries = Vec::new();
        update_codebase_entries_for_file(&root, &changed, &mut entries).unwrap();
        update_codebase_entries_for_file(&root, &removed, &mut entries).unwrap();

        let unchanged_files = vec![changed.clone(), removed.clone()];
        let unchanged = detect_codebase_delta(&root, &unchanged_files, &entries)
            .expect("detect unchanged baseline");
        assert!(unchanged.changed_paths.is_empty());
        assert!(unchanged.removed_paths.is_empty());

        std::fs::write(&changed, "fn changed_with_different_size() {}\n")
            .expect("rewrite changed source");
        std::fs::remove_file(&removed).expect("remove indexed source");
        std::fs::write(&added, "fn added() {}\n").expect("write added source");
        let files = vec![added.clone(), changed.clone()];

        let delta = detect_codebase_delta(&root, &files, &entries).expect("detect delta");
        assert_eq!(delta.changed_paths.len(), 2);
        assert!(delta.changed_paths.contains(&changed));
        assert!(delta.changed_paths.contains(&added));
        assert_eq!(delta.removed_paths, vec!["src/removed.rs".to_string()]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn legacy_index_entries_default_file_fingerprint() {
        let legacy = r#"{
            "path":"src/legacy.rs",
            "kind":"rust-source",
            "summary":"legacy",
            "symbols":[],
            "text":"legacy"
        }"#;
        let entry: CodebaseEntry = serde_json::from_str(legacy).expect("read legacy entry");
        assert_eq!(entry.size_bytes, 0);
        assert_eq!(entry.modified_unix_nanos, 0);
    }

    #[test]
    fn refresh_returns_nonzero_for_fixture_project() {
        let (root, index_path) = fixture_codebase_project("refresh");
        let count = refresh_codebase_index_inner_at(&root, &index_path)
            .expect("refresh fixture should succeed");
        assert!(count > 0, "should have indexed at least one file");
        let entries = read_codebase_entries(&index_path).expect("read fixture index");
        assert!(entries.iter().any(|entry| entry.path == "src/main.rs"));
        assert!(entries.iter().any(|entry| entry.path == "docs/planner.md"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn search_codebase_finds_relevant_fixture_results() {
        let (root, index_path) = fixture_codebase_project("search");
        refresh_codebase_index_inner_at(&root, &index_path).expect("refresh fixture index");
        let results = search_codebase_index(&index_path, "planner agent intent", 5);
        assert!(results.is_ok(), "search should succeed");
        let results = results.unwrap();
        assert!(
            results.iter().any(|r| r.to_lowercase().contains("planner")),
            "planner should appear in results: {:?}",
            results
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
