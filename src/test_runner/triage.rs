//! Failure triage — classify test failures and optionally spawn a Claude
//! Code session to fix the root cause.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::executor::{TestResult, TestStatus};
use super::parser::TestGoal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    UiBug,
    ApiBug,
    Flaky,
    Env,
    Obsolete,
    /// Browser rendered a completely black frame — typically Chrome crashed
    /// mid-test and recovered in headless mode, preventing Flutter/WebGL from
    /// painting.  NOT a code bug; auto-fix must NOT be triggered.
    RenderingFailure,
    Unknown,
}

impl FailureCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UiBug            => "ui_bug",
            Self::ApiBug           => "api_bug",
            Self::Flaky            => "flaky",
            Self::Env              => "env",
            Self::Obsolete         => "obsolete",
            Self::RenderingFailure => "rendering_failure",
            Self::Unknown          => "unknown",
        }
    }

    /// Which repo a session fix should target, if any.
    /// `RenderingFailure` returns `None` — it's a browser/infra issue,
    /// not a code bug; spawning claude_session would waste tokens.
    pub fn fix_repo(&self) -> Option<&'static str> {
        match self {
            Self::UiBug  => Some("frontend"),
            Self::ApiBug => Some("backend"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TriageOutcome {
    pub category: FailureCategory,
    pub reason: String,
    pub auto_fix_triggered: bool,
}

/// Classify a failed test. Checks history memory first (is_flaky), then asks LLM.
pub async fn triage(
    ctx: &crate::adk::context::AgentContext,
    test: &TestGoal,
    result: &TestResult,
    // In-memory run_id — fetches console_log from SQLite for richer context (#221).
    run_id: Option<&str>,
) -> TriageOutcome {
    // Only triage non-passed results
    if matches!(result.status, TestStatus::Passed) {
        return TriageOutcome {
            category: FailureCategory::Unknown,
            reason: "test passed".into(),
            auto_fix_triggered: false,
        };
    }

    // 1. Quick check — historical flakiness
    if super::store::is_flaky(&test.id) {
        return TriageOutcome {
            category: FailureCategory::Flaky,
            reason: "historically flaky (<70% pass rate in last 10 runs)".into(),
            auto_fix_triggered: false,
        };
    }

    // 2. Env check — timeout with zero iterations usually = Chrome/network
    if matches!(result.status, TestStatus::Timeout) && result.iterations < 2 {
        return TriageOutcome {
            category: FailureCategory::Env,
            reason: "timeout before any steps — likely browser/network issue".into(),
            auto_fix_triggered: false,
        };
    }

    // 3. Rendering failure — all-black / near-black screenshot means Chrome
    //    crashed and recovered before Flutter finished rendering (Flutter HTML
    //    renderer initialises asynchronously; a crash mid-init leaves a dark
    //    or blank viewport).
    //    Detected by screenshot file size < 14 KB: truly black PNGs compress
    //    to ~2 KB; near-black "not yet rendered" frames observed at ~12 KB;
    //    real rendered pages (Flutter HTML renderer) are ≥ 15 KB.
    //    Must be checked BEFORE LLM triage to prevent auto-fix being triggered
    //    on non-existent frontend bugs.
    if let Some(ref path) = result.screenshot_path {
        if is_screenshot_all_black(path) {
            tracing::warn!(
                "[triage] '{}' — screenshot is all-black ({} bytes). \
                 Classified as rendering_failure (no auto-fix).",
                test.id,
                std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            );
            return TriageOutcome {
                category: FailureCategory::RenderingFailure,
                reason: "failure screenshot is all-black — Chrome likely recovered in headless \
                         mode during the test; Flutter/WebGL cannot render headless. \
                         Re-run the test; if it fails again check Chrome stability.".into(),
                auto_fix_triggered: false,
            };
        }
    }

    // 4. LLM classification
    let locale = crate::test_runner::i18n::Locale::from_yaml(&test.locale);
    let context = collect_failure_context(test, result);

    // Issue #221: enrich triage context with console errors from SQLite.
    // Even a handful of error messages dramatically improves category accuracy
    // (e.g. "404 /api/products" → api_bug instead of ui_bug).
    let console_section = run_id
        .and_then(|rid| crate::test_runner::store::get_console_log(rid))
        .map(|log_json| {
            // Parse and extract error-level messages only (warnings less useful for classification).
            let msgs: Vec<String> = serde_json::from_str::<serde_json::Value>(&log_json)
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.get("level").and_then(|l| l.as_str()) == Some("error"))
                .take(10)  // cap at 10 to avoid prompt bloat
                .filter_map(|m| m.get("text").and_then(|t| t.as_str()).map(|t| {
                    format!("  [console.error] {}", &t[..t.len().min(200)])
                }))
                .collect();
            if msgs.is_empty() {
                String::new()
            } else {
                format!("\n\nBrowser Console Errors ({} captured):\n{}", msgs.len(), msgs.join("\n"))
            }
        })
        .unwrap_or_default();

    let prompt = format!(r#"{header}

Categories:
{cats}

{context}{console_section}

Output strictly valid JSON (no markdown fence):
{{
  "category": "ui_bug | api_bug | flaky | env | obsolete | rendering_failure",
  "reason": "<{lang} {reason_hint}>",
  "suggested_repo": "frontend | backend | none"
}}
"#,
        header = locale.triage_prompt_header(),
        cats = locale.triage_categories_doc(),
        context = context,
        console_section = console_section,
        lang = locale.reasoning_language(),
        reason_hint = locale.triage_reason_hint(),
    );

    let raw = match crate::llm::call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt).await {
        Ok(s) => s,
        Err(e) => return TriageOutcome {
            category: FailureCategory::Unknown,
            reason: format!("LLM triage failed: {e}"),
            auto_fix_triggered: false,
        },
    };

    let analysis = parse_triage(&raw);
    let category = analysis.category;
    let reason = analysis.reason;

    TriageOutcome {
        category,
        reason,
        auto_fix_triggered: false,  // set by trigger_auto_fix if caller wants
    }
}

/// If the category has a target repo, spawn a claude_session in background.
/// Returns true if a fix was triggered.
///
/// **Dedup**: if an `auto_fix_history` row with `outcome='pending'` exists for
/// this test within the last 30 minutes, skip spawning (records a
/// `skipped_dedupe` entry instead).  This prevents wasting Claude Code tokens
/// when the same bug is reported by consecutive test runs.
///
/// **Outcome tracking**: the spawned thread calls `complete_fix(fix_id, ...)`
/// when claude_session returns, so future callers can query `recent_fixes()`.
pub async fn trigger_auto_fix(test: &TestGoal, result: &TestResult, outcome: &TriageOutcome, run_id: Option<&str>) -> bool {
    let Some(repo) = outcome.category.fix_repo() else { return false; };
    let Some(cwd) = crate::claude_session::repo_path(repo) else {
        tracing::warn!("auto_fix: repo alias '{repo}' not found");
        return false;
    };

    let test_id = test.id.clone();
    let category = outcome.category.as_str().to_string();

    // Dedup check — avoid re-spawning Claude for the same test within 30 minutes
    if super::store::has_pending_fix(&test_id, 30) {
        let _ = super::store::record_skipped_fix(
            &test_id,
            run_id,
            &category,
            "another fix for this test is still pending (within 30 min)",
        );
        tracing::info!(
            "[test_runner] auto_fix for {test_id}: SKIPPED (pending fix exists within 30min)"
        );
        return false;
    }

    // Also skip if the last 3 attempts all failed — probably not fixable by Claude alone
    let recent = super::store::recent_fixes(&test_id, 3);
    if recent.len() >= 3 && recent.iter().all(|f| f.outcome == "failed") {
        let _ = super::store::record_skipped_fix(
            &test_id,
            run_id,
            &category,
            "last 3 auto-fix attempts all failed — giving up",
        );
        tracing::warn!(
            "[test_runner] auto_fix for {test_id}: SKIPPED (3 consecutive failures)"
        );
        return false;
    }

    let bug_prompt = crate::claude_session::build_bug_prompt(
        &format!(
            "Sirin test '{}' failed.\n\nTriage category: {}\nReason: {}\n\n\
             Note: this is triggered automatically by Sirin's test runner. \
             Previous fix attempts (if any) are listed below.",
            test.name,
            outcome.category.as_str(),
            outcome.reason,
        ),
        Some(&test.url),
        result.error_message.as_deref(),
        Some(&format_recent_fix_context(&test_id)),
        result.screenshot_path.as_deref(),
    );

    // Record as pending BEFORE spawning so concurrent callers see it
    let fix_id = match super::store::record_pending_fix(
        &test_id,
        run_id,
        &category,
        &bug_prompt,
    ) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("[test_runner] record_pending_fix failed: {e}");
            return false;
        }
    };

    // Enrich the bug prompt with relevant KB hits BEFORE spawning the
    // (blocking) claude session.  No-op when KB_ENABLED is unset; otherwise
    // prepends up to 3 kbSearch results from the project inferred from cwd
    // so the spawned `claude -p` doesn't rediscover repo conventions.
    let bug_prompt = crate::claude_session::enrich_prompt_with_kb(bug_prompt, &cwd).await;

    std::thread::spawn(move || {
        tracing::info!("[test_runner] auto_fix[{fix_id}]: spawning claude_session in {cwd}");

        let claude_result = crate::claude_session::run_sync(&cwd, &bug_prompt);

        let (exit_code, output) = match claude_result {
            Ok(r) => {
                tracing::info!(
                    "[test_runner] auto_fix[{fix_id}] for {test_id}: exit={}, output chars={}",
                    r.exit_code, r.output.len()
                );
                (r.exit_code as i64, r.output)
            }
            Err(e) => {
                tracing::error!("[test_runner] auto_fix[{fix_id}] for {test_id} failed: {e}");
                (-1, format!("claude_session error: {e}"))
            }
        };

        if let Err(e) = super::store::complete_fix(fix_id, exit_code, &output) {
            tracing::error!("[test_runner] complete_fix({fix_id}) DB write failed: {e}");
        }

        // Verification: only re-run if claude actually succeeded (exit=0)
        // and the test_id corresponds to a real YAML test (not adhoc).
        if exit_code != 0 {
            return;
        }
        if super::parser::find(&test_id).is_none() {
            tracing::info!(
                "[test_runner] auto_fix[{fix_id}]: skipping verification — test_id '{test_id}' \
                 is not a YAML-defined test (probably adhoc)"
            );
            return;
        }

        tracing::info!(
            "[test_runner] auto_fix[{fix_id}]: spawning verification run for {test_id}"
        );
        match super::spawn_run_async(test_id.clone(), false /* no nested auto_fix */) {
            Err(e) => {
                tracing::error!("[test_runner] verification spawn failed: {e}");
            }
            Ok(ver_run_id) => {
                // Poll until terminal state, max 5 minutes
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
                let passed = loop {
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            "[test_runner] verification[{fix_id}]: timed out after 5min"
                        );
                        break None;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    match super::runs::get(&ver_run_id) {
                        Some(state) => match state.phase {
                            super::runs::RunPhase::Complete(r) => {
                                break Some(matches!(r.status, super::executor::TestStatus::Passed));
                            }
                            super::runs::RunPhase::Error(e) => {
                                tracing::warn!(
                                    "[test_runner] verification[{fix_id}]: errored: {e}"
                                );
                                break Some(false);
                            }
                            _ => continue,
                        },
                        None => {
                            tracing::warn!(
                                "[test_runner] verification[{fix_id}]: run_id pruned"
                            );
                            break None;
                        }
                    }
                };

                if let Some(p) = passed {
                    if let Err(e) = super::store::record_verification(fix_id, &ver_run_id, p) {
                        tracing::error!("[test_runner] record_verification failed: {e}");
                    }
                    let label = if p { "VERIFIED" } else { "REGRESSED" };
                    tracing::info!(
                        "[test_runner] auto_fix[{fix_id}] for {test_id}: {label} (ver_run={ver_run_id})"
                    );
                }
            }
        }
    });

    true
}

/// Render the (topic_key, title, content, tags) tuple used by the Issue #34
/// fire-and-forget KB write-back.  Pulled out as a free function so the
/// rendered shape can be unit-tested without spawning a tokio task.
///
/// `category` is the lowercase string form (`ui_bug`, `api_bug`, ...),
/// `auto_fix` mirrors `TriageOutcome::auto_fix_triggered` after the auto-fix
/// branch has run.  `reason` is the LLM (or rule-based) classification reason.
pub fn render_kb_entry(
    test_id: &str,
    category: &str,
    auto_fix: bool,
    reason: &str,
) -> (String, String, String, String) {
    let topic_key = format!("sirin-triage-{test_id}");
    let title     = format!("[TRIAGE] {test_id} → {category}");
    let content   = format!(
        "triage: {category}\n\
         auto_fix_attempted: {auto_fix}\n\
         reason: {reason}"
    );
    let tags = format!("triage,{category}");
    (topic_key, title, content, tags)
}

/// Build a short summary of recent fix attempts to give Claude context.
fn format_recent_fix_context(test_id: &str) -> String {
    let recent = super::store::recent_fixes(test_id, 3);
    if recent.is_empty() {
        return "No previous auto-fix attempts for this test.".into();
    }
    let mut out = String::from("Previous auto-fix attempts:\n");
    for f in &recent {
        out.push_str(&format!(
            "- {} [{}]: outcome={}",
            f.triggered_at, f.category, f.outcome,
        ));
        if let Some(exit) = f.claude_exit_code {
            out.push_str(&format!(" exit={exit}"));
        }
        if let Some(output) = &f.claude_output {
            let snippet: String = output.chars().take(200).collect();
            out.push_str(&format!(" → {snippet}"));
        }
        out.push('\n');
    }
    out
}

// ── Internals ────────────────────────────────────────────────────────────────

fn collect_failure_context(test: &TestGoal, result: &TestResult) -> String {
    let last_steps: Vec<String> = result.history.iter().rev().take(5).rev().enumerate()
        .map(|(i, s)| format!("  {}. action={} obs={}", i + 1,
            truncate(&s.action.to_string(), 120),
            truncate(&s.observation, 200)))
        .collect();

    format!(
        "Test goal: {goal}\nURL: {url}\nStatus: {status:?}\nIterations: {iter}\nError: {err}\n\nLast steps:\n{steps}",
        goal = test.goal.trim(),
        url = test.url,
        status = result.status,
        iter = result.iterations,
        err = result.error_message.as_deref().unwrap_or("(none)"),
        steps = last_steps.join("\n"),
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() }
    else {
        let head: String = s.chars().take(max).collect();
        format!("{head}...")
    }
}

#[derive(Debug)]
struct ParsedTriage {
    category: FailureCategory,
    reason: String,
}

fn parse_triage(raw: &str) -> ParsedTriage {
    let cleaned = strip_fences(raw);
    match serde_json::from_str::<Value>(&cleaned) {
        Ok(v) => {
            let cat = v.get("category").and_then(Value::as_str).unwrap_or("");
            let category = match cat {
                "ui_bug"            => FailureCategory::UiBug,
                "api_bug"           => FailureCategory::ApiBug,
                "flaky"             => FailureCategory::Flaky,
                "env"               => FailureCategory::Env,
                "obsolete"          => FailureCategory::Obsolete,
                "rendering_failure" => FailureCategory::RenderingFailure,
                _                   => FailureCategory::Unknown,
            };
            let reason = v.get("reason").and_then(Value::as_str).unwrap_or("").to_string();
            ParsedTriage { category, reason }
        }
        Err(_) => ParsedTriage {
            category: FailureCategory::Unknown,
            reason: format!("unparseable triage response: {}", truncate(raw, 200)),
        },
    }
}

fn strip_fences(raw: &str) -> String {
    let t = raw.trim();
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let after = after.trim_start_matches('\n');
        if let Some(end) = after.rfind("```") {
            return after[..end].trim().to_string();
        }
    }
    if let (Some(s), Some(e)) = (t.find('{'), t.rfind('}')) {
        if e > s { return t[s..=e].to_string(); }
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_triage() {
        let raw = r#"{"category":"ui_bug","reason":"按鈕沒渲染","suggested_repo":"frontend"}"#;
        let p = parse_triage(raw);
        assert_eq!(p.category, FailureCategory::UiBug);
        assert_eq!(p.reason, "按鈕沒渲染");
    }

    #[test]
    fn parse_unknown_category_maps_to_unknown() {
        let raw = r#"{"category":"weird","reason":"x"}"#;
        let p = parse_triage(raw);
        assert_eq!(p.category, FailureCategory::Unknown);
    }

    #[test]
    fn parse_unparseable_returns_unknown() {
        let raw = "this is not json at all";
        let p = parse_triage(raw);
        assert_eq!(p.category, FailureCategory::Unknown);
    }

    #[test]
    fn fix_repo_mapping() {
        assert_eq!(FailureCategory::UiBug.fix_repo(), Some("frontend"));
        assert_eq!(FailureCategory::ApiBug.fix_repo(), Some("backend"));
        assert_eq!(FailureCategory::Flaky.fix_repo(), None);
        assert_eq!(FailureCategory::Env.fix_repo(), None);
        assert_eq!(FailureCategory::Obsolete.fix_repo(), None);
        // RenderingFailure must NOT trigger auto-fix — not a code bug
        assert_eq!(FailureCategory::RenderingFailure.fix_repo(), None);
    }

    #[test]
    fn parse_rendering_failure_category() {
        let raw = r#"{"category":"rendering_failure","reason":"黑屏","suggested_repo":"none"}"#;
        let p = parse_triage(raw);
        assert_eq!(p.category, FailureCategory::RenderingFailure);
    }

    #[test]
    fn rendering_failure_as_str() {
        assert_eq!(FailureCategory::RenderingFailure.as_str(), "rendering_failure");
    }

    #[test]
    fn is_screenshot_all_black_small_file() {
        // A file < 14 000 bytes → should be detected as black/not rendered
        let dir = std::env::temp_dir();
        let path = dir.join("test_black.png");
        std::fs::write(&path, vec![0u8; 12_000]).unwrap(); // near-black case
        assert!(is_screenshot_all_black(path.to_str().unwrap()));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn is_screenshot_all_black_large_file() {
        // A file ≥ 14 000 bytes → real render, not black
        let dir = std::env::temp_dir();
        let path = dir.join("test_real.png");
        std::fs::write(&path, vec![42u8; 20_000]).unwrap();
        assert!(!is_screenshot_all_black(path.to_str().unwrap()));
        let _ = std::fs::remove_file(path);
    }

    // ── Issue #34: triage → KB rendering ────────────────────────────────

    #[test]
    fn render_kb_entry_basic_ui_bug() {
        let (topic, title, content, tags) =
            render_kb_entry("agora-pickup-flow", "ui_bug", true, "按鈕沒渲染");
        assert_eq!(topic, "sirin-triage-agora-pickup-flow");
        assert_eq!(title, "[TRIAGE] agora-pickup-flow → ui_bug");
        assert!(content.contains("triage: ui_bug"),         "got: {content}");
        assert!(content.contains("auto_fix_attempted: true"), "got: {content}");
        assert!(content.contains("reason: 按鈕沒渲染"),       "got: {content}");
        assert_eq!(tags, "triage,ui_bug");
    }

    #[test]
    fn render_kb_entry_no_auto_fix() {
        let (_topic, _title, content, tags) =
            render_kb_entry("flaky-test", "flaky", false, "<70% pass rate");
        assert!(content.contains("auto_fix_attempted: false"), "got: {content}");
        assert_eq!(tags, "triage,flaky");
    }

    #[test]
    fn render_kb_entry_rendering_failure_no_auto_fix() {
        // RenderingFailure must never trigger auto-fix — the tag must reflect
        // the category accurately so kbSearch("rendering_failure") finds it.
        let (_topic, title, content, tags) =
            render_kb_entry("k14-exact", "rendering_failure", false, "all-black screenshot");
        assert!(title.contains("rendering_failure"));
        assert!(content.contains("auto_fix_attempted: false"));
        assert_eq!(tags, "triage,rendering_failure");
    }

    #[test]
    fn strip_fences_removes_markdown() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("prefix {\"a\":1} suffix"), "{\"a\":1}");
    }
}

/// Returns `true` if the screenshot at `path` is all-black.
///
/// Uses file-size heuristic: all-black PNGs compress to ≤ 3 KB; real
/// rendered pages produce ≥ 15 KB.  Threshold 8 KB gives safe margin.
fn is_screenshot_all_black(path: &str) -> bool {
    // Threshold mirrors executor.rs: 14 000 bytes.
    // Near-black screenshots (Chrome recovered, Flutter not yet rendered)
    // observed at ~12 KB — above the old 8 KB threshold.
    // Real rendered pages (Flutter HTML renderer) are ≥ 15 KB.
    std::fs::metadata(path)
        .map(|m| m.len() < 14_000)
        .unwrap_or(false)
}

// Remove unused json! import warning
#[allow(dead_code)]
fn _uses_json_import() -> Value { json!({}) }
