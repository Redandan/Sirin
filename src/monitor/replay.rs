//! Trace file replay — load a historical NDJSON trace and parse into events.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::monitor::events::ServerEvent;

/// List all `.sirin/trace-*.ndjson` files, newest first.
pub fn list_trace_files() -> Vec<PathBuf> {
    let dir = Path::new(".sirin");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().map_or(false, |ext| ext == "ndjson")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map_or(false, |n| n.starts_with("trace-"))
        })
        .collect();
    // Sort newest first by filename (ISO8601 names sort lexicographically = time order)
    files.sort_by(|a, b| b.cmp(a));
    files
}

/// Load a trace file and parse each line as a `ServerEvent`.
/// Lines that fail to parse are silently skipped.
pub fn load_trace(path: &Path) -> Vec<ServerEvent> {
    let Ok(file) = std::fs::File::open(path) else {
        return vec![];
    };
    let reader = BufReader::new(file);
    reader
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ServerEvent>(&l).ok())
        .collect()
}

/// Short display name for a trace file path.
/// `.sirin/trace-2026-04-17T10:30:00.ndjson` → `"2026-04-17 10:30:00"`
pub fn display_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("trace");
    stem.trim_start_matches("trace-")
        .replace('T', " ")
        .trim_end_matches(".000Z")
        .to_string()
}
