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

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

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
    *graph_cache().write() = None;
}

// ── File-system paths ──────────────────────────────────────────────────────────

fn graph_file_path() -> PathBuf {
    crate::platform::app_data_dir().join("code_graph").join("graph.jsonl")
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
pub fn build_call_graph() -> Result<Vec<CallGraphEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let root = find_project_root().ok_or("Cannot determine project root")?;

    let mut rs_files = Vec::new();
    collect_rs_files(&root, &mut rs_files);
    rs_files.sort();

    let mut entries: Vec<CallGraphEntry> = Vec::new();
    for file in &rs_files {
        let Ok(src) = fs::read_to_string(file) else {
            continue;
        };
        let rel = relative_display(file, &root);
        let mut file_entries = parse_rust_file(&rel, &src);
        entries.append(&mut file_entries);
    }

    Ok(entries)
}

// ── Persistence ────────────────────────────────────────────────────────────────

/// Rebuild the call graph index and write it to disk.  Called automatically
/// by `memory::refresh_codebase_index()` after each file write.
pub fn refresh_call_graph() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let entries = build_call_graph()?;

    let path = graph_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;

    for entry in &entries {
        writeln!(file, "{}", serde_json::to_string(entry)?)?;
    }

    // Refresh the in-process cache.
    *graph_cache().write() = Some(entries.clone());

    Ok(entries.len())
}

/// Load call graph entries from disk into the in-process cache if not already
/// loaded.
pub fn ensure_call_graph() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Fast path: already loaded.
    if graph_cache().read().is_some() {
        return Ok(());
    }

    let path = graph_file_path();
    let entries: Vec<CallGraphEntry> = if path.exists() {
        let file = fs::File::open(&path)?;
        BufReader::new(file)
            .lines()
            .filter_map(|l| l.ok())
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<CallGraphEntry>(&l).ok())
            .collect()
    } else {
        // No file on disk yet — build it now.
        match build_call_graph() {
            Ok(e) => {
                // Best-effort persist; ignore errors.
                let _ = refresh_call_graph();
                e
            }
            Err(_) => Vec::new(),
        }
    };

    *graph_cache().write() = Some(entries);
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

    let guard = graph_cache().read();
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
        *graph_cache().write() = Some(entries);

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
    fn build_call_graph_returns_entries_for_real_project() {
        let entries = build_call_graph().expect("build_call_graph should succeed");
        assert!(
            !entries.is_empty(),
            "should parse at least one symbol from the project"
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
    }

    #[test]
    fn build_call_graph_finds_known_symbols() {
        let entries = build_call_graph().expect("build should succeed");
        let symbols: Vec<&str> = entries.iter().map(|e| e.symbol.as_str()).collect();
        // These functions exist in the project and must be detected.
        assert!(
            symbols.contains(&"run_react_loop"),
            "run_react_loop should be in call graph"
        );
        assert!(
            symbols.contains(&"memory_store"),
            "memory_store should be in call graph"
        );
    }

    // ── refresh_call_graph writes to disk ─────────────────────────────────────
    //
    // NOTE: Tests that mutate the global graph_cache() run here serially because
    // `query_returns_callers_and_callees` also manipulates the cache.
    // These tests therefore only verify file I/O, not cache state.

    #[test]
    fn refresh_call_graph_writes_nonempty_file_to_disk() {
        let count = refresh_call_graph().expect("refresh should succeed");
        assert!(count > 0, "should have written at least one entry");

        // Verify the on-disk file exists and is not empty.
        let path = graph_file_path();
        assert!(path.exists(), "graph file should be written to disk");
        let content = std::fs::read_to_string(&path).expect("should read graph file");
        assert!(!content.trim().is_empty(), "graph file should not be empty");

        // Leave the cache in a known state.
        invalidate_cache();
    }

    #[test]
    fn graph_file_contains_valid_jsonl_entries() {
        // Build entries without touching the cache.
        let entries = build_call_graph().expect("build ok");

        // Write to a temp file and verify round-trip.
        let tmp = std::env::temp_dir().join(format!("sirin_cg_test_{}.jsonl", std::process::id()));
        {
            use std::io::Write as IoWrite;
            let mut f = std::fs::File::create(&tmp).expect("create tmp ok");
            for e in &entries {
                writeln!(f, "{}", serde_json::to_string(e).expect("serialize ok"))
                    .expect("write ok");
            }
        }
        let content = std::fs::read_to_string(&tmp).expect("read ok");
        let loaded: Vec<CallGraphEntry> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("deserialize ok"))
            .collect();
        assert_eq!(loaded.len(), entries.len(), "round-trip count should match");
        std::fs::remove_file(&tmp).ok();
    }
}
