//! Aggregate Anthropic API token usage from worker session jsonl logs.

use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Sonnet 4.6 standard tier pricing per million tokens (USD).
const PRICE_INPUT: f64       = 3.00;
const PRICE_OUTPUT: f64      = 15.00;
const PRICE_CACHE_READ: f64  = 0.30;
const PRICE_CACHE_WRITE: f64 = 3.75;

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageSnapshot {
    pub window_secs:    u64,
    pub api_calls:      u64,
    pub input_tokens:   u64,
    pub output_tokens:  u64,
    pub cache_read:     u64,
    pub cache_write:    u64,
    pub total_tokens:   u64,
    pub tokens_per_min: u64,
    pub cost_usd:       f64,
    pub cost_per_hour:  f64,
    pub cache_hit_pct:  f64, // cache_read / (cache_read + cache_write + input_tokens)
}

/// Cached snapshot to avoid reparsing jsonl on every UI frame.
static CACHE: OnceLock<Mutex<(Instant, UsageSnapshot)>> = OnceLock::new();
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Compute token usage over the last `window_secs` across all squad worker sessions.
/// Returns the cached snapshot if it was computed within the last 5 seconds.
pub fn snapshot(window_secs: u64) -> UsageSnapshot {
    let cell = CACHE.get_or_init(|| {
        Mutex::new((Instant::now() - Duration::from_secs(60), Default::default()))
    });
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if guard.0.elapsed() < CACHE_TTL && guard.1.window_secs == window_secs {
        return guard.1.clone();
    }
    let snap = compute(window_secs);
    *guard = (Instant::now(), snap.clone());
    snap
}

pub fn compute(window_secs: u64) -> UsageSnapshot {
    use chrono::Utc;

    let cutoff = Utc::now() - chrono::Duration::seconds(window_secs as i64);
    let cutoff_str = cutoff.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let session_ids = collect_session_ids();
    let log_dir     = anthropic_project_log_dir();

    let mut snap = UsageSnapshot { window_secs, ..Default::default() };

    for sid in &session_ids {
        let log = log_dir.join(format!("{}.jsonl", sid));
        let Ok(text) = fs::read_to_string(&log) else { continue };
        for line in text.lines() {
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let Some(ts) = obj.get("timestamp").and_then(|v| v.as_str()) else { continue };
            if ts < cutoff_str.as_str() { continue; }
            let Some(usage) = obj.get("message").and_then(|m| m.get("usage")) else { continue };
            snap.api_calls     += 1;
            snap.input_tokens  += usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            snap.output_tokens += usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            snap.cache_read    += usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            snap.cache_write   += usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        }
    }

    snap.total_tokens   = snap.input_tokens + snap.output_tokens + snap.cache_read + snap.cache_write;
    snap.tokens_per_min = if window_secs > 0 { snap.total_tokens * 60 / window_secs } else { 0 };

    let m = 1_000_000.0_f64;
    snap.cost_usd = (snap.input_tokens  as f64 * PRICE_INPUT
                   + snap.output_tokens as f64 * PRICE_OUTPUT
                   + snap.cache_read    as f64 * PRICE_CACHE_READ
                   + snap.cache_write   as f64 * PRICE_CACHE_WRITE) / m;
    snap.cost_per_hour = if window_secs > 0 {
        snap.cost_usd * 3600.0 / window_secs as f64
    } else {
        0.0
    };

    let denom = snap.cache_read + snap.cache_write + snap.input_tokens;
    snap.cache_hit_pct = if denom > 0 {
        snap.cache_read as f64 * 100.0 / denom as f64
    } else {
        0.0
    };

    snap
}

/// Read all worker state files in data/multi_agent/, return their session_ids.
fn collect_session_ids() -> Vec<String> {
    let dir = crate::platform::app_data_dir().join("data").join("multi_agent");
    let Ok(entries) = fs::read_dir(&dir) else { return vec![]; };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        if path.file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.starts_with("task_queue"))
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(obj)  = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        if let Some(sid) = obj.get("session_id").and_then(|v| v.as_str()) {
            out.push(sid.to_string());
        }
    }
    out
}

/// Locate ~/.claude/projects/<encoded-cwd>/ — where Claude CLI writes session jsonl logs.
/// The encoded-cwd format replaces ':', '\', '/' with '-'.
fn anthropic_project_log_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let encoded = cwd.to_string_lossy().replace([':', '\\', '/'], "-");
    home.join(".claude").join("projects").join(encoded)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_when_no_session_logs() {
        // In test builds, app_data_dir() → "test_data" (no actual session files)
        // so compute() should return all zeros without panicking.
        let snap = compute(300);
        assert_eq!(snap.api_calls, 0);
        assert_eq!(snap.input_tokens, 0);
        assert_eq!(snap.output_tokens, 0);
        assert_eq!(snap.total_tokens, 0);
        assert_eq!(snap.tokens_per_min, 0);
        assert_eq!(snap.cost_usd, 0.0);
        assert_eq!(snap.cost_per_hour, 0.0);
        assert_eq!(snap.cache_hit_pct, 0.0);
    }

    #[test]
    fn cost_formula_one_million_each() {
        // Manually build a snapshot with 1M input + 1M output.
        // Expected cost: $3.00 + $15.00 = $18.00
        let mut snap = UsageSnapshot { window_secs: 3600, ..Default::default() };
        snap.input_tokens  = 1_000_000;
        snap.output_tokens = 1_000_000;
        snap.total_tokens  = snap.input_tokens + snap.output_tokens;

        let m = 1_000_000.0_f64;
        snap.cost_usd = (snap.input_tokens  as f64 * PRICE_INPUT
                       + snap.output_tokens as f64 * PRICE_OUTPUT) / m;

        let expected = 3.00 + 15.00; // $18
        let delta = (snap.cost_usd - expected).abs();
        assert!(delta < 1e-9, "cost_usd={} expected={}", snap.cost_usd, expected);
    }
}
