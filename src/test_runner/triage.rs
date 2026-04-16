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
    Unknown,
}

impl FailureCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UiBug    => "ui_bug",
            Self::ApiBug   => "api_bug",
            Self::Flaky    => "flaky",
            Self::Env      => "env",
            Self::Obsolete => "obsolete",
            Self::Unknown  => "unknown",
        }
    }

    /// Which repo a session fix should target, if any.
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

    // 3. LLM classification
    let locale = crate::test_runner::i18n::Locale::from_yaml(&test.locale);
    let context = collect_failure_context(test, result);
    let prompt = format!(r#"{header}

Categories:
{cats}

{context}

Output strictly valid JSON (no markdown fence):
{{
  "category": "ui_bug | api_bug | flaky | env | obsolete",
  "reason": "<{lang} {reason_hint}>",
  "suggested_repo": "frontend | backend | none"
}}
"#,
        header = locale.triage_prompt_header(),
        cats = locale.triage_categories_doc(),
        context = context,
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
pub fn trigger_auto_fix(test: &TestGoal, result: &TestResult, outcome: &TriageOutcome) -> bool {
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
            None,
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
            None,
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
        None,  // TODO: thread run_id through
        &category,
        &bug_prompt,
    ) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("[test_runner] record_pending_fix failed: {e}");
            return false;
        }
    };

    std::thread::spawn(move || {
        tracing::info!("[test_runner] auto_fix[{fix_id}]: spawning claude_session in {cwd}");
        match crate::claude_session::run_sync(&cwd, &bug_prompt) {
            Ok(r) => {
                tracing::info!(
                    "[test_runner] auto_fix[{fix_id}] for {test_id}: exit={}, output chars={}",
                    r.exit_code, r.output.len()
                );
                if let Err(e) = super::store::complete_fix(fix_id, r.exit_code as i64, &r.output) {
                    tracing::error!("[test_runner] complete_fix({fix_id}) DB write failed: {e}");
                }
            }
            Err(e) => {
                tracing::error!("[test_runner] auto_fix[{fix_id}] for {test_id} failed: {e}");
                let _ = super::store::complete_fix(fix_id, -1, &format!("claude_session error: {e}"));
            }
        }
    });

    true
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
                "ui_bug"   => FailureCategory::UiBug,
                "api_bug"  => FailureCategory::ApiBug,
                "flaky"    => FailureCategory::Flaky,
                "env"      => FailureCategory::Env,
                "obsolete" => FailureCategory::Obsolete,
                _          => FailureCategory::Unknown,
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
    }

    #[test]
    fn strip_fences_removes_markdown() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("prefix {\"a\":1} suffix"), "{\"a\":1}");
    }
}

// Remove unused json! import warning
#[allow(dead_code)]
fn _uses_json_import() -> Value { json!({}) }
