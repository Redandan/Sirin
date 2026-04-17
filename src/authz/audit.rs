/// NDJSON audit log writer.
///
/// Each authorization decision is written as a single JSON line to a file.
/// Append-only; no rotation in this PR (rotation is a follow-up task).
///
/// Event format (§7 of DESIGN_AUTHZ.md):
///
/// ```json
/// {"ts":"2026-04-17T03:14:15.123Z","type":"allow","client":"claude-code@0.3.2",
///  "action":"ax_click","args":{"backend_id":42},
///  "url":"https://redandan.github.io/#/wallet","rule":"readonly_allow"}
/// ```

use chrono::Utc;
use serde_json::{json, Value as JsonValue};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

// ─── Public helpers ───────────────────────────────────────────────────────────

/// Rotate `log_path` once it exceeds 10 MiB, keeping up to 5 backups.
///
/// Rotation order: `.5` is deleted, `.4`→`.5`, `.3`→`.4`, `.2`→`.3`,
/// `.1`→`.2`, `audit.ndjson`→`.1`.  A fresh file is then opened by the
/// caller.
fn maybe_rotate(path: &str) {
    let p = std::path::Path::new(path);
    if p.metadata().map(|m| m.len()).unwrap_or(0) < 10 * 1024 * 1024 {
        return;
    }
    // Delete the oldest backup first to make room.
    let oldest = format!("{path}.5");
    let _ = std::fs::remove_file(&oldest);
    // Shift backups: .4 → .5, .3 → .4, .2 → .3, .1 → .2
    for i in (1..5usize).rev() {
        let from = format!("{path}.{i}");
        let to   = format!("{path}.{}", i + 1);
        let _ = std::fs::rename(&from, &to);
    }
    // Current log → .1
    let _ = std::fs::rename(path, &format!("{path}.1"));
}

/// Write a single NDJSON event to the audit log at `log_path`.
///
/// Creates the file (and parent dirs) if it doesn't exist.
/// Rotates the file if it has grown beyond 10 MiB (keeps 5 backups).
/// Silently swallows write errors (audit failure must not block the call).
pub fn append_event(log_path: &str, event: &JsonValue) {
    let path = Path::new(log_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Size-based rotation before opening for append.
    maybe_rotate(log_path);

    let mut line = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');

    let file = OpenOptions::new().create(true).append(true).open(path);
    if let Ok(mut f) = file {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Log an `allow` decision.
pub fn log_allow(
    log_path: &str,
    client: &str,
    action: &str,
    args: &JsonValue,
    url: &Option<String>,
    rule: &str,
) {
    let ev = json!({
        "ts":     Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "type":   "allow",
        "client": client,
        "action": action,
        "args":   args,
        "url":    url.as_deref().unwrap_or(""),
        "rule":   rule,
    });
    append_event(log_path, &ev);
}

/// Log a `deny` decision.
pub fn log_deny(
    log_path: &str,
    client: &str,
    action: &str,
    args: &JsonValue,
    url: &Option<String>,
    rule: &str,
) {
    let ev = json!({
        "ts":     Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "type":   "deny",
        "client": client,
        "action": action,
        "args":   args,
        "url":    url.as_deref().unwrap_or(""),
        "rule":   rule,
    });
    append_event(log_path, &ev);
}

/// Log an `ask` event (before the human responds).
pub fn log_ask(
    log_path: &str,
    client: &str,
    action: &str,
    args: &JsonValue,
    url: &Option<String>,
    rule: &str,
) {
    let ev = json!({
        "ts":     Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "type":   "ask",
        "client": client,
        "action": action,
        "args":   args,
        "url":    url.as_deref().unwrap_or(""),
        "rule":   rule,
    });
    append_event(log_path, &ev);
}

/// Log a `learn` event when a new rule is persisted.
pub fn log_learn(
    log_path: &str,
    client: &str,
    new_rule: &JsonValue,
    written_to: &str,
) {
    let ev = json!({
        "ts":         Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "type":       "learn",
        "client":     client,
        "new_rule":   new_rule,
        "written_to": written_to,
    });
    append_event(log_path, &ev);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod audit_test {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_log_path() -> String {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("authz_audit_{}_{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir.join("audit.ndjson").to_string_lossy().to_string()
    }

    fn parse_lines(path: &str) -> Vec<serde_json::Value> {
        let content = fs::read_to_string(path).unwrap_or_default();
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("valid JSON line"))
            .collect()
    }

    #[test]
    fn write_allow_event_and_parse_back() {
        let path = tmp_log_path();
        log_allow(
            &path,
            "claude-code@0.3.2",
            "ax_click",
            &json!({ "backend_id": 42 }),
            &Some("https://redandan.github.io/#/wallet".into()),
            "ax_* url=redandan.github.io/**",
        );

        let lines = parse_lines(&path);
        assert_eq!(lines.len(), 1);
        let ev = &lines[0];
        assert_eq!(ev["type"], "allow");
        assert_eq!(ev["client"], "claude-code@0.3.2");
        assert_eq!(ev["action"], "ax_click");
        assert_eq!(ev["args"]["backend_id"], 42);
        assert_eq!(ev["url"], "https://redandan.github.io/#/wallet");
        assert_eq!(ev["rule"], "ax_* url=redandan.github.io/**");
        // ts is present and non-empty
        assert!(ev["ts"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
    }

    #[test]
    fn write_deny_event_and_parse_back() {
        let path = tmp_log_path();
        log_deny(
            &path,
            "claude-desktop@1.2.0",
            "goto",
            &json!({ "target": "https://paypal.com/login" }),
            &Some("about:blank".into()),
            "url=**paypal**",
        );

        let lines = parse_lines(&path);
        assert_eq!(lines.len(), 1);
        let ev = &lines[0];
        assert_eq!(ev["type"], "deny");
        assert_eq!(ev["action"], "goto");
        assert_eq!(ev["args"]["target"], "https://paypal.com/login");
    }

    #[test]
    fn write_ask_event_and_parse_back() {
        let path = tmp_log_path();
        log_ask(
            &path,
            "cursor@0.50",
            "goto",
            &json!({ "target": "https://google.com/oauth" }),
            &Some("about:blank".into()),
            "action=goto url=**.google.com/**",
        );

        let lines = parse_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "ask");
    }

    #[test]
    fn write_multiple_events_all_parseable() {
        let path = tmp_log_path();
        log_allow(&path, "a@1", "screenshot", &json!({}), &None, "readonly_allow");
        log_deny(&path, "b@1", "eval", &json!({ "target": "document.cookie" }), &None, "js_contains=document.cookie");
        log_ask(&path, "c@1", "goto", &json!({ "target": "https://google.com/" }), &None, "ask_rule");

        let lines = parse_lines(&path);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["type"], "allow");
        assert_eq!(lines[1]["type"], "deny");
        assert_eq!(lines[2]["type"], "ask");
    }

    #[test]
    fn write_learn_event_and_parse_back() {
        let path = tmp_log_path();
        log_learn(
            &path,
            "claude-code@0.3.2",
            &json!({ "action": "goto", "url_pattern": "https://docs.flutter.dev/**" }),
            ".sirin/authz.yaml",
        );

        let lines = parse_lines(&path);
        assert_eq!(lines.len(), 1);
        let ev = &lines[0];
        assert_eq!(ev["type"], "learn");
        assert_eq!(ev["written_to"], ".sirin/authz.yaml");
        assert_eq!(ev["new_rule"]["action"], "goto");
    }

    #[test]
    fn creates_parent_dirs_if_missing() {
        let dir = std::env::temp_dir().join(format!("authz_deep_{}", std::process::id()));
        let path = dir.join("nested").join("deeply").join("audit.ndjson");
        let path_str = path.to_string_lossy().to_string();

        log_allow(&path_str, "test@1", "screenshot", &json!({}), &None, "readonly_allow");
        assert!(path.exists(), "file should be created in nested dirs");
    }

    #[test]
    fn url_none_writes_empty_string() {
        let path = tmp_log_path();
        log_allow(&path, "test@1", "ax_tree", &json!({}), &None, "readonly_allow");
        let lines = parse_lines(&path);
        assert_eq!(lines[0]["url"], "");
    }
}
