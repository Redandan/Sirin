//! NDJSON trace writer with file rotation.
//!
//! Each Sirin session creates one `TraceWriter`.  Every `ServerEvent` is
//! serialized as a single JSON line and appended to the file.
//!
//! ## Rotation
//! When the current file exceeds `size_limit` bytes the writer rotates:
//!
//! ```text
//! trace-<ts>.ndjson      ← current (reset to empty)
//! trace-<ts>.ndjson.1    ← was current
//! trace-<ts>.ndjson.2    ← was .1
//! …
//! trace-<ts>.ndjson.20   ← oldest retained
//! trace-<ts>.ndjson.21+  ← deleted
//! ```
//!
//! The default limit is 100 MiB; tests pass a smaller value.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::events::ServerEvent;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Default rotation threshold: 100 MiB.
pub const DEFAULT_SIZE_LIMIT: u64 = 100 * 1024 * 1024;

/// Maximum number of rotated files to keep alongside the live file.
pub const MAX_ROTATIONS: u32 = 20;

// ── TraceWriter ───────────────────────────────────────────────────────────────

/// Appends `ServerEvent` lines to an NDJSON file with rotation.
pub struct TraceWriter {
    /// Path to the currently-open file (e.g. `…/.sirin/trace-<ts>.ndjson`).
    path: PathBuf,
    /// Open file handle (append mode).
    file: File,
    /// How many bytes have been written to the current file.
    bytes_written: u64,
    /// Rotate when `bytes_written` exceeds this threshold.
    size_limit: u64,
}

impl TraceWriter {
    /// Open (or create) the trace file at `path`.
    ///
    /// The parent directory is created if it does not exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_with_limit(path, DEFAULT_SIZE_LIMIT)
    }

    /// Same as `open`, but with a custom `size_limit` (useful for testing).
    pub fn open_with_limit(path: impl AsRef<Path>, size_limit: u64) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("trace_writer: mkdir {:?}: {e}", parent))?;
        }

        let file = open_append(&path)?;
        let bytes_written = file
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0);

        Ok(Self { path, file, bytes_written, size_limit })
    }

    /// Serialize `event` as a JSON line and append it to the trace file.
    ///
    /// Rotates the file first if the current size would exceed `size_limit`.
    pub fn write_event(&mut self, event: &ServerEvent) -> Result<(), String> {
        let line = serde_json::to_string(event)
            .map_err(|e| format!("trace_writer: serialize: {e}"))?;

        // Rotate before writing if the new line would push us over the limit.
        // (We compare against the current size before appending.)
        let line_bytes = (line.len() + 1) as u64; // +1 for '\n'
        if self.bytes_written + line_bytes > self.size_limit && self.bytes_written > 0 {
            self.rotate()?;
        }

        writeln!(self.file, "{}", line)
            .map_err(|e| format!("trace_writer: write: {e}"))?;
        self.file
            .flush()
            .map_err(|e| format!("trace_writer: flush: {e}"))?;

        self.bytes_written += line_bytes;
        Ok(())
    }

    /// Current byte count of the live file (approximate — updated at each write).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Rotate: shift `.N` → `.N+1` (dropping > MAX_ROTATIONS), rename live →
    /// `.1`, then re-open the live path as a fresh empty file.
    fn rotate(&mut self) -> Result<(), String> {
        // Drop the current file handle so we can rename it.
        // We re-open at the end.
        // We can't move `self.file` without a replacement, so close via flush first
        // (the File will be replaced by re-open below).
        self.file
            .flush()
            .map_err(|e| format!("trace_writer: pre-rotate flush: {e}"))?;

        // Shift existing rotated files:  .20 → deleted, .19 → .20, …, .1 → .2
        for n in (1..=MAX_ROTATIONS).rev() {
            let from = rotated_path(&self.path, n);
            let to = rotated_path(&self.path, n + 1);

            if from.exists() {
                if n == MAX_ROTATIONS {
                    // Delete the oldest
                    fs::remove_file(&from)
                        .map_err(|e| format!("trace_writer: remove {:?}: {e}", from))?;
                } else {
                    fs::rename(&from, &to)
                        .map_err(|e| format!("trace_writer: rename {:?} → {:?}: {e}", from, to))?;
                }
            }
        }

        // Rename live file to .1
        let backup = rotated_path(&self.path, 1);
        fs::rename(&self.path, &backup)
            .map_err(|e| format!("trace_writer: rename live → .1: {e}"))?;

        // Re-open a fresh live file
        self.file = open_append(&self.path)?;
        self.bytes_written = 0;
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn open_append(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("trace_writer: open {:?}: {e}", path))
}

fn rotated_path(base: &Path, n: u32) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::io::Read as IoRead;

    /// Create a unique temp directory for one test (no external crate needed).
    fn test_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir()
            .join("sirin_monitor_tests")
            .join(format!("{}_{}", name, std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn sample_event() -> ServerEvent {
        ServerEvent::UrlChange {
            ts: Utc::now(),
            url: "https://example.com/test".into(),
        }
    }

    fn line_count(path: &Path) -> usize {
        let mut s = String::new();
        File::open(path).unwrap().read_to_string(&mut s).unwrap();
        s.lines().filter(|l| !l.is_empty()).count()
    }

    // ── Basic write ──────────────────────────────────────────────────────────

    #[test]
    fn writes_ndjson_lines() {
        let dir = test_dir("writes_ndjson_lines");
        let path = dir.join("trace.ndjson");
        let mut w = TraceWriter::open_with_limit(&path, DEFAULT_SIZE_LIMIT).unwrap();

        w.write_event(&sample_event()).unwrap();
        w.write_event(&sample_event()).unwrap();

        assert_eq!(line_count(&path), 2);
        assert!(w.bytes_written() > 0);
    }

    #[test]
    fn written_lines_are_valid_json() {
        let dir = test_dir("valid_json");
        let path = dir.join("trace.ndjson");
        let mut w = TraceWriter::open_with_limit(&path, DEFAULT_SIZE_LIMIT).unwrap();
        w.write_event(&sample_event()).unwrap();

        let mut s = String::new();
        File::open(&path).unwrap().read_to_string(&mut s).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(parsed["type"], "url_change");
    }

    // ── Small-file, no rotation ───────────────────────────────────────────────

    #[test]
    fn small_file_does_not_rotate() {
        let dir = test_dir("no_rotate");
        let path = dir.join("trace.ndjson");
        // Very large limit — rotation must NOT trigger
        let mut w = TraceWriter::open_with_limit(&path, DEFAULT_SIZE_LIMIT).unwrap();
        w.write_event(&sample_event()).unwrap();

        // No .1 file should exist
        assert!(
            !rotated_path(&path, 1).exists(),
            "rotation must not happen for small file"
        );
    }

    // ── Rotation on size overflow ─────────────────────────────────────────────

    #[test]
    fn rotation_triggers_when_limit_exceeded() {
        let dir = test_dir("rotation_triggers");
        let path = dir.join("trace.ndjson");
        // Limit of 1 byte → every subsequent write triggers rotation
        let mut w = TraceWriter::open_with_limit(&path, 1).unwrap();

        w.write_event(&sample_event()).unwrap(); // first write — file is empty, goes in
        w.write_event(&sample_event()).unwrap(); // triggers rotation before writing

        // The live file should have the second line; .1 should have the first
        assert!(rotated_path(&path, 1).exists(), ".1 must exist after rotation");
        assert_eq!(line_count(&path), 1, "live file should have 1 line after rotation");
        assert_eq!(
            line_count(&rotated_path(&path, 1)),
            1,
            ".1 should have 1 line"
        );
    }

    #[test]
    fn rotation_shifts_existing_rotations() {
        let dir = test_dir("rotation_shifts");
        let path = dir.join("trace.ndjson");
        let limit = 1u64; // force rotation on every second write
        let mut w = TraceWriter::open_with_limit(&path, limit).unwrap();

        // Write 3 events → 2 rotations → files: live, .1, .2
        for _ in 0..3 {
            w.write_event(&sample_event()).unwrap();
        }

        assert!(rotated_path(&path, 1).exists(), ".1 must exist");
        assert!(rotated_path(&path, 2).exists(), ".2 must exist");
    }

    // ── MAX_ROTATIONS cap ─────────────────────────────────────────────────────

    #[test]
    fn rotation_cap_does_not_exceed_max() {
        let dir = test_dir("rotation_cap");
        let path = dir.join("trace.ndjson");
        let limit = 1u64;
        let mut w = TraceWriter::open_with_limit(&path, limit).unwrap();

        // Write MAX_ROTATIONS + 5 more events to force many rotations.
        // We need MAX_ROTATIONS + 2 writes: first write fills the file,
        // each subsequent write triggers one rotation.
        for _ in 0..(MAX_ROTATIONS + 5) {
            w.write_event(&sample_event()).unwrap();
        }

        // No file beyond .MAX_ROTATIONS should exist
        assert!(
            !rotated_path(&path, MAX_ROTATIONS + 1).exists(),
            ".{} must not exist", MAX_ROTATIONS + 1
        );
        assert!(
            rotated_path(&path, MAX_ROTATIONS).exists(),
            ".{} must exist", MAX_ROTATIONS
        );
    }

    // ── Parent dir is created automatically ─────────────────────────────────

    #[test]
    fn creates_parent_directory() {
        let dir = test_dir("creates_parent");
        let path = dir.join("nested").join("deep").join("trace.ndjson");
        let mut w = TraceWriter::open_with_limit(&path, DEFAULT_SIZE_LIMIT).unwrap();
        w.write_event(&sample_event()).unwrap();
        assert!(path.exists());
    }
}
