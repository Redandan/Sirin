//! Generic JSONL-backed append-only log.
//!
//! Unifies the five ad-hoc persistence layers that were writing one-JSON-per-line
//! files (task tracker, research log, pending replies, conversation context,
//! and eventually anything else) with identical semantics:
//!
//! - Append one entry per line.
//! - Tail read (`read_last_n`) for UI / feed views.
//! - Atomic rewrite via `.tmp` + `rename` for updates / deletes / trim.
//! - Per-instance `Mutex` so concurrent callers on the same log serialise.
//!
//! Concurrency note: a single `JsonlLog<T>` instance is safe to share across
//! threads.  Two separate `JsonlLog<T>` pointing to the *same file* will NOT
//! coordinate with each other — callers wanting that should clone the log
//! instead of constructing a new one.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(thiserror::Error, Debug)]
pub enum JsonlError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("lock poisoned")]
    Lock,
}

/// A typed JSONL log file.  Cheap to clone (shares inner mutex + path).
pub struct JsonlLog<T> {
    inner: Arc<LogInner>,
    _phantom: PhantomData<fn() -> T>,
}

struct LogInner {
    path: PathBuf,
    lock: Mutex<()>,
}

impl<T> Clone for JsonlLog<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _phantom: PhantomData,
        }
    }
}

impl<T> JsonlLog<T>
where
    T: Serialize + DeserializeOwned,
{
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(LogInner {
                path: path.into(),
                lock: Mutex::new(()),
            }),
            _phantom: PhantomData,
        }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, JsonlError> {
        self.inner.lock.lock().map_err(|_| JsonlError::Lock)
    }

    /// Append one entry (create the file + parent dirs if needed).
    pub fn append(&self, entry: &T) -> Result<(), JsonlError> {
        let _g = self.lock()?;
        if let Some(parent) = self.inner.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(entry)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.inner.path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Read every parseable entry.  Unparseable lines are silently skipped so
    /// one corrupt line can't take down the whole log — the UI would rather
    /// see partial history than crash.
    pub fn read_all(&self) -> Result<Vec<T>, JsonlError> {
        let _g = self.lock()?;
        self.read_all_unlocked()
    }

    /// Read only the last `n` entries.  The whole file is parsed but the
    /// caller-visible slice is bounded, so the signature stays simple.
    pub fn read_last_n(&self, n: usize) -> Result<Vec<T>, JsonlError> {
        let all = self.read_all()?;
        let start = all.len().saturating_sub(n);
        Ok(all.into_iter().skip(start).collect())
    }

    /// Atomically rewrite the log with the result of `f`.
    ///
    /// `f` receives ownership of every currently-persisted entry and returns
    /// the new entry list.  Implemented via `.tmp` write + rename so crashes
    /// can never leave a half-written file.
    pub fn rewrite_with<F>(&self, f: F) -> Result<(), JsonlError>
    where
        F: FnOnce(Vec<T>) -> Vec<T>,
    {
        let _g = self.lock()?;
        let existing = self.read_all_unlocked()?;
        let updated = f(existing);
        self.atomic_write_unlocked(&updated)
    }

    /// Insert-or-replace by key.
    ///
    /// If an existing entry has the same key as `entry` (determined by
    /// `key_fn`), it is replaced; otherwise `entry` is appended.
    pub fn upsert_by<K, F>(&self, entry: T, key_fn: F) -> Result<(), JsonlError>
    where
        K: PartialEq,
        F: Fn(&T) -> K,
        T: Clone,
    {
        let target_key = key_fn(&entry);
        self.rewrite_with(move |mut entries| {
            let mut found = false;
            for slot in entries.iter_mut() {
                if key_fn(slot) == target_key {
                    *slot = entry.clone();
                    found = true;
                    break;
                }
            }
            if !found {
                entries.push(entry);
            }
            entries
        })
    }

    /// Keep only the newest `max_lines` entries, discarding oldest.
    /// Returns the number of entries removed (`0` if no trim was needed).
    pub fn trim_to_max(&self, max_lines: usize) -> Result<usize, JsonlError> {
        let _g = self.lock()?;
        let existing = self.read_all_unlocked()?;
        if existing.len() <= max_lines {
            return Ok(0);
        }
        let removed = existing.len() - max_lines;
        let keep: Vec<T> = existing.into_iter().skip(removed).collect();
        self.atomic_write_unlocked(&keep)?;
        Ok(removed)
    }

    // ── Internal helpers (assume lock already held) ──────────────────────────

    fn read_all_unlocked(&self) -> Result<Vec<T>, JsonlError> {
        if !self.inner.path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&self.inner.path)?;
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<T>(&line) {
                out.push(entry);
            }
        }
        Ok(out)
    }

    fn atomic_write_unlocked(&self, entries: &[T]) -> Result<(), JsonlError> {
        if let Some(parent) = self.inner.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.inner.path.with_extension("jsonl.tmp");
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            for entry in entries {
                let line = serde_json::to_string(entry)?;
                writeln!(file, "{line}")?;
            }
        }
        fs::rename(&tmp, &self.inner.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    struct TestEntry {
        id: String,
        value: i32,
    }

    fn tmp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sirin_jsonl_test_{}_{}.jsonl",
            std::process::id(),
            label
        ))
    }

    #[test]
    fn append_and_read_roundtrip() {
        let p = tmp_path("append");
        let _ = fs::remove_file(&p);
        let log: JsonlLog<TestEntry> = JsonlLog::new(&p);
        log.append(&TestEntry {
            id: "a".into(),
            value: 1,
        })
        .unwrap();
        log.append(&TestEntry {
            id: "b".into(),
            value: 2,
        })
        .unwrap();
        let all = log.read_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "a");
        assert_eq!(all[1].value, 2);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn missing_file_returns_empty() {
        let p = tmp_path("missing");
        let _ = fs::remove_file(&p);
        let log: JsonlLog<TestEntry> = JsonlLog::new(&p);
        assert!(log.read_all().unwrap().is_empty());
        assert!(log.read_last_n(10).unwrap().is_empty());
    }

    #[test]
    fn read_last_n_returns_tail() {
        let p = tmp_path("tail");
        let _ = fs::remove_file(&p);
        let log: JsonlLog<TestEntry> = JsonlLog::new(&p);
        for i in 0..10 {
            log.append(&TestEntry {
                id: format!("e{i}"),
                value: i,
            })
            .unwrap();
        }
        let tail = log.read_last_n(3).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[2].value, 9);
        assert_eq!(tail[0].value, 7);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn rewrite_with_transforms_all_entries() {
        let p = tmp_path("rewrite");
        let _ = fs::remove_file(&p);
        let log: JsonlLog<TestEntry> = JsonlLog::new(&p);
        log.append(&TestEntry {
            id: "a".into(),
            value: 1,
        })
        .unwrap();
        log.append(&TestEntry {
            id: "b".into(),
            value: 2,
        })
        .unwrap();
        log.rewrite_with(|entries| {
            entries
                .into_iter()
                .map(|mut e| {
                    e.value *= 10;
                    e
                })
                .collect()
        })
        .unwrap();
        let all = log.read_all().unwrap();
        assert_eq!(all[0].value, 10);
        assert_eq!(all[1].value, 20);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn upsert_by_replaces_existing() {
        let p = tmp_path("upsert_replace");
        let _ = fs::remove_file(&p);
        let log: JsonlLog<TestEntry> = JsonlLog::new(&p);
        log.append(&TestEntry {
            id: "a".into(),
            value: 1,
        })
        .unwrap();
        log.upsert_by(
            TestEntry {
                id: "a".into(),
                value: 99,
            },
            |e| e.id.clone(),
        )
        .unwrap();
        let all = log.read_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].value, 99);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn upsert_by_appends_when_missing() {
        let p = tmp_path("upsert_append");
        let _ = fs::remove_file(&p);
        let log: JsonlLog<TestEntry> = JsonlLog::new(&p);
        log.upsert_by(
            TestEntry {
                id: "new".into(),
                value: 5,
            },
            |e| e.id.clone(),
        )
        .unwrap();
        let all = log.read_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "new");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn trim_to_max_keeps_newest() {
        let p = tmp_path("trim");
        let _ = fs::remove_file(&p);
        let log: JsonlLog<TestEntry> = JsonlLog::new(&p);
        for i in 0..8 {
            log.append(&TestEntry {
                id: format!("e{i}"),
                value: i,
            })
            .unwrap();
        }
        let removed = log.trim_to_max(5).unwrap();
        assert_eq!(removed, 3);
        let all = log.read_all().unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].value, 3, "oldest kept should be entry 3");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn trim_noop_when_under_limit() {
        let p = tmp_path("trim_noop");
        let _ = fs::remove_file(&p);
        let log: JsonlLog<TestEntry> = JsonlLog::new(&p);
        for i in 0..3 {
            log.append(&TestEntry {
                id: format!("e{i}"),
                value: i,
            })
            .unwrap();
        }
        assert_eq!(log.trim_to_max(10).unwrap(), 0);
        let _ = fs::remove_file(&p);
    }
}
