//! Call-graph index for Rust source files, built with tree-sitter.
//!
//! ## Concurrency
//! Graph and freshness stamp live in separate `RwLock`s.  `query_call_graph`
//! takes only the read lock; `refresh_call_graph` / `invalidate_cache` take
//! the write lock while rescanning.  The rescan itself is single-threaded —
//! calling it concurrently will serialise at the lock.
//!
//! ## Layers
//!
//! 1. **Parse** (`parse_rust_file`) — given a single `.rs` file, extract
//!    every top-level symbol (fn / struct / enum / trait / impl methods) and,
//!    for every function, the list of callee names it directly invokes.
//!
//! 2. **Build** (`build_call_graph`) — scan all `.rs` files under the
//!    project root, merge the per-file parse results, and construct a
//!    project-wide reverse index (`called_by`).
//!
//! 3. **Persist** (`save_call_graph` / `load_call_graph`) — serialise the
//!    graph to a JSONL file at `data/code_graph/graph.jsonl` (or the platform
//!    data directory) so it survives across restarts.
//!
//! 4. **Query** (`query_call_graph`) — given a symbol name and hop depth,
//!    return its callers, callees, and definition location.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use std::sync::RwLock;

// ── Public types ───────────────────────────────────────────────────────────────

/// One symbol extracted from a source file together with its direct callees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraphEntry {
    /// Repo-relative path with forward slashes.
    pub path: String,
    /// Short name of the symbol (e.g. `run_react_loop`).
    pub symbol: String,
    /// `"fn"` | `"struct"` | `"enum"` | `"trait"`.
    pub kind: String,
    /// 1-based line number of the definition.
    pub line: usize,
    /// Names of symbols directly called within this function body.
    pub calls: Vec<String>,
}

/// Result returned by `query_call_graph`.
pub struct CallGraphResult {
    /// `"path:line"` of the symbol's definition, if found.
    pub defined_in: Option<String>,
    /// `"path::symbol"` strings for all functions that call this symbol.
    pub callers: Vec<String>,
    /// Short names of all symbols this function calls.
    pub callees: Vec<String>,
}

// ── In-process cache ───────────────────────────────────────────────────────────

/// All call graph entries loaded from disk, cached in memory.
fn graph_cache() -> &'static RwLock<Option<Vec<CallGraphEntry>>> {
    static CACHE: OnceLock<RwLock<Option<Vec<CallGraphEntry>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(None))
}

/// Invalidate the in-process cache so the next query reloads from disk.
pub fn invalidate_cache() {
    *graph_cache().write().unwrap_or_else(|e| e.into_inner()) = None;
}

// ── File-system paths ──────────────────────────────────────────────────────────

fn graph_file_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("code_graph")
        .join("graph.jsonl")
}

fn relative_display(path: &Path, root: &Path) -> String {
    let canonical_root = fs::canonicalize(root).ok();
    let canonical_path = fs::canonicalize(path).ok();

    let display = canonical_path
        .as_deref()
        .zip(canonical_root.as_deref())
        .and_then(|(p, r)| p.strip_prefix(r).ok().map(Path::to_path_buf))
        .or_else(|| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf());

    display.to_string_lossy().replace('\\', "/")
}

fn find_project_root() -> Option<PathBuf> {
    // Walk up from CWD until we find a Cargo.toml.
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("Cargo.toml").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ── Tree-sitter parse ──────────────────────────────────────────────────────────

/// Parse a single Rust source file and extract symbol / call information.
///
/// Returns an empty `Vec` if tree-sitter fails to parse the file.
pub fn parse_rust_file(rel_path: &str, src: &str) -> Vec<CallGraphEntry> {
    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_rust::language();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut entries = Vec::new();
    collect_items(rel_path, &tree.root_node(), src.as_bytes(), &mut entries);
    entries
}

// ── AST traversal ─────────────────────────────────────────────────────────────

fn collect_items(
    path: &str,
    node: &tree_sitter::Node,
    src: &[u8],
    entries: &mut Vec<CallGraphEntry>,
) {
    match node.kind() {
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let symbol = name_node.utf8_text(src).unwrap_or("?").to_string();
                let line = name_node.start_position().row + 1;

                let mut calls = Vec::new();
                if let Some(body) = node.child_by_field_name("body") {
                    collect_calls(&body, src, &mut calls);
                }
                calls.sort();
                calls.dedup();

                entries.push(CallGraphEntry {
                    path: path.to_string(),
                    symbol,
                    kind: "fn".to_string(),
                    line,
                    calls,
                });
            }
        }

        "struct_item" | "enum_item" | "trait_item" => {
            let kind = match node.kind() {
                "struct_item" => "struct",
                "enum_item" => "enum",
                _ => "trait",
            };
            if let Some(name_node) = node.child_by_field_name("name") {
                let symbol = name_node.utf8_text(src).unwrap_or("?").to_string();
                let line = name_node.start_position().row + 1;
                entries.push(CallGraphEntry {
                    path: path.to_string(),
                    symbol,
                    kind: kind.to_string(),
                    line,
                    calls: Vec::new(),
                });
            }
            // Recurse into trait bodies so their associated fn items are captured.
            if node.kind() == "trait_item" {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    collect_items(path, &child, src, entries);
                }
            }
        }

        // `impl` blocks: recurse into their declaration lists so we capture
        // all the methods inside them.
        "impl_item" | "source_file" | "declaration_list" | "block" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_items(path, &child, src, entries);
            }
        }

        _ => {
            // For any other container node, keep descending.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_items(path, &child, src, entries);
            }
        }
    }
}

/// Walk `node` recursively and push every directly-called symbol name into
/// `calls`.
fn collect_calls(node: &tree_sitter::Node, src: &[u8], calls: &mut Vec<String>) {
    match node.kind() {
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                if let Some(name) = callee_name(&func, src) {
                    calls.push(name);
                }
            }
        }
        "method_call_expression" => {
            if let Some(method) = node.child_by_field_name("method") {
                if let Ok(name) = method.utf8_text(src) {
                    calls.push(name.to_string());
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_calls(&child, src, calls);
    }
}

/// Extract the short callee name from a `call_expression`'s function node.
fn callee_name(node: &tree_sitter::Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(src).ok().map(str::to_string),
        "scoped_identifier" => {
            // Prefer the leaf `name` field; fall back to the whole text.
            node.child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok().map(str::to_string))
                .or_else(|| node.utf8_text(src).ok().map(str::to_string))
        }
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(src).ok().map(str::to_string)),
        _ => None,
    }
}

// ── Project-wide build ─────────────────────────────────────────────────────────

fn collect_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let skip = [".git", "target", "node_modules", ".next", "dist"];
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if skip.contains(&name_str.as_ref()) {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Parse all `.rs` files under the project root and return the combined list
/// of `CallGraphEntry` records, ready to be persisted.
fn build_call_graph_from_root(
    root: &Path,
) -> Result<Vec<CallGraphEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let mut rs_files = Vec::new();
    collect_rs_files(root, &mut rs_files);
    rs_files.sort();

    let mut entries: Vec<CallGraphEntry> = Vec::new();
    for file in &rs_files {
        let Ok(src) = fs::read_to_string(file) else {
            continue;
        };
        let rel = relative_display(file, root);
        let mut file_entries = parse_rust_file(&rel, &src);
        entries.append(&mut file_entries);
    }

    Ok(entries)
}

pub fn build_call_graph() -> Result<Vec<CallGraphEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let root = find_project_root().ok_or("Cannot determine project root")?;
    build_call_graph_from_root(&root)
}

fn update_entries_for_file(
    root: &Path,
    path: &Path,
    entries: &mut Vec<CallGraphEntry>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rel = relative_display(path, root);
    entries.retain(|entry| entry.path != rel);

    if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
        let src = fs::read_to_string(path)?;
        entries.extend(parse_rust_file(&rel, &src));
    }

    entries.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    Ok(())
}

fn update_entries_for_files(
    root: &Path,
    changed_paths: &[PathBuf],
    removed_paths: &[String],
    entries: &mut Vec<CallGraphEntry>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let removed: HashSet<&str> = removed_paths.iter().map(String::as_str).collect();
    entries.retain(|entry| !removed.contains(entry.path.as_str()));
    for path in changed_paths {
        update_entries_for_file(root, path, entries)?;
    }
    Ok(())
}

// ── Persistence ────────────────────────────────────────────────────────────────

fn persist_call_graph_to(
    path: &Path,
    entries: &[CallGraphEntry],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    crate::jsonl_log::atomic_write_jsonl(path, entries)?;
    Ok(())
}

fn persist_call_graph(
    entries: &[CallGraphEntry],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    persist_call_graph_to(&graph_file_path(), entries)
}

fn rebuild_call_graph_to(
    root: &Path,
    path: &Path,
) -> Result<Vec<CallGraphEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let entries = build_call_graph_from_root(root)?;
    persist_call_graph_to(path, &entries)?;
    Ok(entries)
}

/// Rebuild the complete call graph index and write it to disk. Initial index
/// creation and explicit rebuilds use this path; normal coding-tool writes use
/// [`refresh_call_graph_file`], while stale codebase scans batch external deltas.
pub fn refresh_call_graph() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let root = find_project_root().ok_or("Cannot determine project root")?;
    let entries = rebuild_call_graph_to(&root, &graph_file_path())?;

    // Refresh the in-process cache.
    *graph_cache().write().unwrap_or_else(|e| e.into_inner()) = Some(entries.clone());

    Ok(entries.len())
}

/// Refresh only the call-graph entries owned by one changed source file.
///
/// The first call still builds the full graph when no persisted index exists.
/// Subsequent coding-tool writes parse one Rust file and rewrite the compact
/// JSONL index, avoiding a full project tree-sitter scan after every hunk.
pub fn refresh_call_graph_file(
    path: &Path,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
        return Ok(0);
    }

    refresh_call_graph_files(&[path.to_path_buf()], &[])
}

/// Apply a batch of external Rust-file changes and deletions, then persist one
/// graph snapshot. This is used by the periodic codebase delta scan so several
/// externally-edited files do not each rewrite the full JSONL graph.
pub(crate) fn refresh_call_graph_files(
    changed_paths: &[PathBuf],
    removed_paths: &[String],
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    if changed_paths.is_empty() && removed_paths.is_empty() {
        ensure_call_graph()?;
        return Ok(graph_cache()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(Vec::len)
            .unwrap_or(0));
    }

    let root = find_project_root().ok_or("Cannot determine project root")?;
    let canonical_root = fs::canonicalize(&root)?;
    let mut canonical_paths = Vec::new();
    for path in changed_paths {
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let canonical_path = fs::canonicalize(path)?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(format!(
                "call graph refresh path '{}' is outside project root '{}'",
                canonical_path.display(),
                canonical_root.display()
            )
            .into());
        }
        canonical_paths.push(canonical_path);
    }

    ensure_call_graph()?;
    let mut guard = graph_cache().write().unwrap_or_else(|e| e.into_inner());
    let entries = guard.get_or_insert_with(Vec::new);
    update_entries_for_files(&canonical_root, &canonical_paths, removed_paths, entries)?;
    persist_call_graph(entries)?;
    Ok(entries.len())
}

/// Load call graph entries from disk into the in-process cache if not already
/// loaded.
pub fn ensure_call_graph() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Fast path: already loaded.
    if graph_cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
    {
        return Ok(());
    }

    let path = graph_file_path();
    let entries: Vec<CallGraphEntry> = if path.exists() {
        let file = fs::File::open(&path)?;
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<CallGraphEntry>(&l).ok())
            .collect()
    } else {
        // No file on disk yet — build once and persist that same snapshot.
        let entries = build_call_graph()?;
        persist_call_graph(&entries)?;
        entries
    };

    *graph_cache().write().unwrap_or_else(|e| e.into_inner()) = Some(entries);
    Ok(())
}

// ── Query ──────────────────────────────────────────────────────────────────────

/// Query the call graph for a given `symbol` up to `hops` hops away.
///
/// * `hops = 1` returns direct callers and callees only.
/// * Higher values expand the neighbourhood transitively (capped at 3).
///
/// If the graph has not yet been built, this will trigger a synchronous build.
pub fn query_call_graph(
    symbol: &str,
    hops: usize,
) -> Result<CallGraphResult, Box<dyn std::error::Error + Send + Sync>> {
    let _ = ensure_call_graph();

    let guard = graph_cache().read().unwrap_or_else(|e| e.into_inner());
    let entries = match guard.as_deref() {
        Some(e) => e,
        None => {
            return Ok(CallGraphResult {
                defined_in: None,
                callers: Vec::new(),
                callees: Vec::new(),
            })
        }
    };

    // Build a quick lookup map: symbol → entries.
    let by_symbol: HashMap<&str, Vec<&CallGraphEntry>> = {
        let mut map: HashMap<&str, Vec<&CallGraphEntry>> = HashMap::new();
        for e in entries {
            map.entry(e.symbol.as_str()).or_default().push(e);
        }
        map
    };

    // Find the canonical entry for the requested symbol.
    let root_entries: &[&CallGraphEntry] =
        by_symbol.get(symbol).map(|v| v.as_slice()).unwrap_or(&[]);
    let defined_in = root_entries
        .first()
        .map(|e| format!("{}:{}", e.path, e.line));

    // Collect callees: everything the symbol calls directly (1-hop).
    let mut callees: Vec<String> = root_entries
        .iter()
        .flat_map(|e| e.calls.iter().cloned())
        .collect();
    callees.sort();
    callees.dedup();

    // Expand callees transitively for hops > 1.
    if hops > 1 {
        let mut seen: std::collections::HashSet<String> = callees.iter().cloned().collect();
        seen.insert(symbol.to_string());
        let mut frontier: Vec<String> = callees.clone();

        for _ in 1..hops {
            let mut next_frontier = Vec::new();
            for name in &frontier {
                if let Some(sub) = by_symbol.get(name.as_str()) {
                    for s in sub.iter().flat_map(|e| e.calls.iter()) {
                        if seen.insert(s.clone()) {
                            callees.push(s.clone());
                            next_frontier.push(s.clone());
                        }
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }
        callees.sort();
    }

    // Callers: every function whose `calls` list contains `symbol`.
    let mut callers: Vec<String> = entries
        .iter()
        .filter(|e| e.calls.iter().any(|c| c == symbol))
        .map(|e| format!("{}::{}", e.path, e.symbol))
        .collect();
    callers.sort();
    callers.dedup();

    // Expand callers transitively for hops > 1.
    if hops > 1 {
        let mut seen: std::collections::HashSet<String> = callers.iter().cloned().collect();
        let mut frontier: Vec<String> = callers
            .iter()
            .filter_map(|c| c.split("::").last().map(str::to_string))
            .collect();

        for _ in 1..hops {
            let mut next_frontier = Vec::new();
            for name in &frontier {
                let new_callers: Vec<String> = entries
                    .iter()
                    .filter(|e| e.calls.iter().any(|c| c == name))
                    .map(|e| format!("{}::{}", e.path, e.symbol))
                    .filter(|key| seen.insert(key.clone()))
                    .collect();
                for c in &new_callers {
                    callers.push(c.clone());
                    if let Some(short) = c.split("::").last() {
                        next_frontier.push(short.to_string());
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }
        callers.sort();
    }

    Ok(CallGraphResult {
        defined_in,
        callers,
        callees,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SIMPLE_SRC: &str = r#"
pub fn greet(name: &str) -> String {
    format_greeting(name)
}

fn format_greeting(name: &str) -> String {
    format!("Hello, {name}!")
}

struct Greeter;

enum Color { Red, Green, Blue }
"#;

    fn fixture_project(label: &str) -> PathBuf {
        static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sirin_call_graph_{label}_{}_{}",
            std::process::id(),
            sequence
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("src")).expect("create fixture source tree");
        std::fs::write(
            root.join("src/planner_agent.rs"),
            "pub fn run_react_loop() { memory_store(); }\n",
        )
        .expect("write planner fixture");
        std::fs::write(
            root.join("src/memory.rs"),
            "pub fn memory_store() {}\npub struct MemoryIndex;\n",
        )
        .expect("write memory fixture");
        root
    }

    #[test]
    fn parse_extracts_functions() {
        let entries = parse_rust_file("test.rs", SIMPLE_SRC);
        let fns: Vec<&str> = entries
            .iter()
            .filter(|e| e.kind == "fn")
            .map(|e| e.symbol.as_str())
            .collect();
        assert!(fns.contains(&"greet"), "expected 'greet', got {fns:?}");
        assert!(
            fns.contains(&"format_greeting"),
            "expected 'format_greeting', got {fns:?}"
        );
    }

    #[test]
    fn parse_extracts_structs_and_enums() {
        let entries = parse_rust_file("test.rs", SIMPLE_SRC);
        let kinds: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.symbol.as_str(), e.kind.as_str()))
            .collect();
        assert!(kinds.contains(&("Greeter", "struct")));
        assert!(kinds.contains(&("Color", "enum")));
    }

    #[test]
    fn parse_extracts_calls() {
        let entries = parse_rust_file("test.rs", SIMPLE_SRC);
        let greet = entries.iter().find(|e| e.symbol == "greet").unwrap();
        assert!(
            greet.calls.contains(&"format_greeting".to_string()),
            "expected 'format_greeting' in calls, got {:?}",
            greet.calls
        );
    }

    #[test]
    fn query_returns_callers_and_callees() {
        // Manually populate the cache with the parsed entries.
        let entries = parse_rust_file("test.rs", SIMPLE_SRC);
        *graph_cache().write().unwrap_or_else(|e| e.into_inner()) = Some(entries);

        let result = query_call_graph("greet", 1).unwrap();
        assert!(
            result.callees.contains(&"format_greeting".to_string()),
            "callees: {:?}",
            result.callees
        );

        let result2 = query_call_graph("format_greeting", 1).unwrap();
        assert!(
            result2.callers.iter().any(|c| c.contains("greet")),
            "callers: {:?}",
            result2.callers
        );

        // Clean up the cache after the test.
        invalidate_cache();
    }

    // ── build_call_graph ──────────────────────────────────────────────────────

    #[test]
    fn build_call_graph_returns_entries_for_fixture_project() {
        let root = fixture_project("build_entries");
        let entries = build_call_graph_from_root(&root).expect("build_call_graph should succeed");
        assert!(
            !entries.is_empty(),
            "should parse at least one symbol from the fixture project"
        );
        // Every entry must have a non-empty symbol and path.
        assert!(
            entries.iter().all(|e| !e.symbol.is_empty()),
            "all symbols should be non-empty"
        );
        assert!(
            entries.iter().all(|e| !e.path.is_empty()),
            "all paths should be non-empty"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn build_call_graph_finds_known_fixture_symbols() {
        let root = fixture_project("known_symbols");
        let entries = build_call_graph_from_root(&root).expect("build should succeed");
        let symbols: Vec<&str> = entries.iter().map(|e| e.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"run_react_loop"),
            "run_react_loop should be in call graph"
        );
        assert!(
            symbols.contains(&"memory_store"),
            "memory_store should be in call graph"
        );
        let run_loop = entries
            .iter()
            .find(|entry| entry.symbol == "run_react_loop")
            .expect("run_react_loop entry");
        assert!(run_loop.calls.contains(&"memory_store".to_string()));
        std::fs::remove_dir_all(&root).ok();
    }

    // ── refresh_call_graph writes to disk ─────────────────────────────────────
    // Explicit fixture paths keep persistence tests isolated from the global
    // cache and the user's app-data graph.

    #[test]
    fn fixture_refresh_writes_nonempty_file_to_disk() {
        let root = fixture_project("refresh");
        let path = root.join("data/code_graph/graph.jsonl");
        let entries = rebuild_call_graph_to(&root, &path).expect("refresh should succeed");
        assert!(
            !entries.is_empty(),
            "should have written at least one entry"
        );

        assert!(path.exists(), "graph file should be written to disk");
        let content = std::fs::read_to_string(&path).expect("should read graph file");
        assert!(!content.trim().is_empty(), "graph file should not be empty");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn fixture_graph_file_contains_valid_jsonl_entries() {
        let root = fixture_project("jsonl");
        let path = root.join("graph.jsonl");
        let entries = rebuild_call_graph_to(&root, &path).expect("build and persist ok");
        let content = std::fs::read_to_string(&path).expect("read ok");
        let loaded: Vec<CallGraphEntry> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("deserialize ok"))
            .collect();
        assert_eq!(loaded.len(), entries.len(), "round-trip count should match");
        assert!(loaded.iter().any(|entry| entry.symbol == "run_react_loop"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn incremental_refresh_replaces_only_the_changed_file() {
        let root = std::env::temp_dir().join(format!(
            "sirin_call_graph_incremental_{}",
            std::process::id()
        ));
        let src_dir = root.join("src");
        let changed = src_dir.join("changed.rs");
        let untouched = src_dir.join("untouched.rs");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&src_dir).expect("create temp source tree");
        std::fs::write(&changed, "fn old_symbol() { shared(); }\n").expect("write changed");
        std::fs::write(&untouched, "fn untouched_symbol() {}\n").expect("write untouched");

        let mut entries = parse_rust_file(
            "src/changed.rs",
            &std::fs::read_to_string(&changed).unwrap(),
        );
        entries.extend(parse_rust_file(
            "src/untouched.rs",
            &std::fs::read_to_string(&untouched).unwrap(),
        ));

        std::fs::write(&changed, "fn new_symbol() { shared(); }\n").expect("rewrite changed");
        update_entries_for_file(&root, &changed, &mut entries).expect("incremental refresh");

        assert!(entries.iter().any(|entry| entry.symbol == "new_symbol"));
        assert!(!entries.iter().any(|entry| entry.symbol == "old_symbol"));
        assert!(entries
            .iter()
            .any(|entry| entry.symbol == "untouched_symbol"));

        update_entries_for_files(
            &root,
            &[changed],
            &["src/untouched.rs".to_string()],
            &mut entries,
        )
        .expect("batch delta should update and remove entries");
        assert!(entries.iter().any(|entry| entry.symbol == "new_symbol"));
        assert!(!entries
            .iter()
            .any(|entry| entry.symbol == "untouched_symbol"));
        std::fs::remove_dir_all(&root).ok();
    }
}
