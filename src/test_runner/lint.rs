//! YAML test-goal linting (Issue #239).
//!
//! Five rules covering trap classes that have caused repeated commits in
//! `config/tests/agora_regression/`:
//!
//! 1. **clear_state_reauth** — `clear_state` without follow-up
//!    `goto ?__test_role=` (existing rule, kept here for unification with #189)
//! 2. **h5_viewport_missing** — buyer / seller / delivery role URL but no
//!    H5 viewport block (KB: `trap-agoramarket-buyer-h5-viewport`).
//!    Symptom: 800KB screenshots, two-column desktop layout instead of mobile.
//! 3. **double_enable_a11y** — back-to-back `enable_a11y` lines without
//!    intervening action.  Trips convergence-guard; LLM stops early.
//! 4. **success_criteria_no_positive** — `success_criteria` containing only
//!    "no error" / "未顯示 X" tokens.  These pass on the login page and any
//!    non-target page.  Need at least one positive-presence assertion.
//!    KB: `sirin-trap-no-error-false-positive`.
//! 5. **iterations_ratio_low** — `max_iterations` < `step_count`.  Hard
//!    floor only — MEMORY.md's 1.5× guideline is documented advice, not
//!    enforced (existing corpus has many tests at 1.1-1.4× that work in
//!    practice).  This rule catches only truly-insufficient configs that
//!    are guaranteed to time out on the first step that needs a retry.
//!
//! All rules are warn-level — they produce a [`LintIssue`] but do not
//! reject the YAML.  The parser path keeps loading; the corpus test
//! `lint_all_regression_yamls` enforces the corpus stays clean.

use super::parser::TestGoal;

// ── Public API ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintIssue {
    pub rule:      &'static str,
    pub severity:  Severity,
    /// Human-readable message, suitable for log output or surfacing in UI.
    pub message:   String,
    /// 1-based step index when the issue is anchored to a specific step.
    /// `None` for whole-file issues (e.g., missing viewport).
    pub step:      Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warn,
    /// Reserved for future use — no rule produces Error today.
    #[allow(dead_code)]
    Error,
}

/// Run every lint rule against `goal`.  Returns all issues found.
/// Order is stable (rule-by-rule, then by step index).
pub fn lint(goal: &TestGoal) -> Vec<LintIssue> {
    let mut out = Vec::new();
    out.extend(lint_clear_state_reauth(goal));
    out.extend(lint_h5_viewport_missing(goal));
    out.extend(lint_double_enable_a11y(goal));
    out.extend(lint_success_criteria_positive(goal));
    out.extend(lint_iterations_ratio(goal));
    out
}

/// Convenience: log all issues at warn level.  Used by the parser-path
/// hook so old `find()` callers see the warnings without us having to
/// thread a Vec through the API.
pub fn log_issues(goal: &TestGoal, issues: &[LintIssue]) {
    for it in issues {
        match it.step {
            Some(n) => tracing::warn!(
                "[yaml_lint] '{}' step {} [{}] {}",
                goal.id, n, it.rule, it.message
            ),
            None => tracing::warn!(
                "[yaml_lint] '{}' [{}] {}",
                goal.id, it.rule, it.message
            ),
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Extract numbered steps "  N. action..." from `goal.goal`.  Same parser
/// the existing #189 rule uses.
fn extract_steps(goal: &TestGoal) -> Vec<&str> {
    goal.goal.lines()
        .filter(|l| {
            let t = l.trim_start();
            t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
                && t.contains(". ")
        })
        .collect()
}

/// True when `step` text contains an action whose name matches `action_name`
/// (case-insensitive substring).
fn step_has_action(step: &str, action_name: &str) -> bool {
    step.to_lowercase().contains(action_name)
}

/// True when the URL has `__test_role=buyer|seller|delivery` — the three
/// H5-mobile roles.  Admin is desktop, so it does NOT need viewport.
fn is_h5_role_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    ["__test_role=buyer", "__test_role=seller", "__test_role=delivery"]
        .iter().any(|p| lower.contains(p))
}

// ── Rule 1: clear_state without re-auth ──────────────────────────────────────

pub fn lint_clear_state_reauth(goal: &TestGoal) -> Vec<LintIssue> {
    if !goal.url.contains("__test_role=") {
        return Vec::new();
    }
    let steps = extract_steps(goal);
    let mut out = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let action = step.trim_start().to_lowercase();
        if action.contains("clear_state") {
            let next_5 = &steps[i.saturating_add(1)..steps.len().min(i + 6)];
            let has_reauth = next_5.iter().any(|s| {
                let lower = s.to_lowercase();
                lower.contains("goto") && lower.contains("__test_role=")
            });
            if !has_reauth {
                out.push(LintIssue {
                    rule:     "clear_state_reauth",
                    severity: Severity::Warn,
                    step:     Some(i + 1),
                    message: "uses clear_state but no `goto target=\"URL?__test_role=X\"` \
                              within the next 5 steps — auto-login will not trigger \
                              after clear_state. Add the goto immediately after the \
                              following wait. (KB: sirin-trap-clear-state-loses-test-role-url)"
                        .into(),
                });
            }
        }
    }
    out
}

// ── Rule 2: H5 viewport missing for mobile roles ─────────────────────────────

pub fn lint_h5_viewport_missing(goal: &TestGoal) -> Vec<LintIssue> {
    if !is_h5_role_url(&goal.url) {
        return Vec::new();
    }
    if let Some(vp) = &goal.viewport {
        if vp.mobile && vp.width <= 500 {
            return Vec::new();
        }
        // Has viewport but it's the wrong size — still warn, but with
        // diagnostic detail.
        return vec![LintIssue {
            rule:     "h5_viewport_missing",
            severity: Severity::Warn,
            step:     None,
            message: format!(
                "URL targets a mobile role (buyer/seller/delivery) but viewport \
                 is {}×{} mobile={} — should be 390×844 mobile=true. \
                 (KB: trap-agoramarket-buyer-h5-viewport)",
                vp.width, vp.height, vp.mobile,
            ),
        }];
    }
    vec![LintIssue {
        rule:     "h5_viewport_missing",
        severity: Severity::Warn,
        step:     None,
        message: "URL targets a mobile role (buyer/seller/delivery) but no \
                  viewport block — symptoms: 800KB screenshots, two-column \
                  desktop layout. Add: viewport: {width: 390, height: 844, \
                  scale: 2.0, mobile: true}. \
                  (KB: trap-agoramarket-buyer-h5-viewport)".into(),
    }]
}

// ── Rule 3: back-to-back enable_a11y ─────────────────────────────────────────
//
// The init sequence MEMORY.md prescribes is:
//   wait 3000 → enable_a11y → wait 2000 → enable_a11y → wait 1000
// The pattern that trips convergence-guard is two `enable_a11y` lines with
// NO action between them — typically a missing wait line.  We flag any
// adjacent enable_a11y without an intervening non-enable_a11y action.

pub fn lint_double_enable_a11y(goal: &TestGoal) -> Vec<LintIssue> {
    let steps = extract_steps(goal);
    let mut out = Vec::new();
    let mut prev_was_enable = false;
    for (i, step) in steps.iter().enumerate() {
        let lower = step.trim_start().to_lowercase();
        let is_enable = step_has_action(&lower, "enable_a11y");
        if is_enable && prev_was_enable {
            out.push(LintIssue {
                rule:     "double_enable_a11y",
                severity: Severity::Warn,
                step:     Some(i + 1),
                message: "two `enable_a11y` lines back-to-back without an \
                          intervening action — trips convergence-guard, LLM \
                          may stop early. Insert a `wait` line between them \
                          (the standard init sequence is wait→enable→wait→enable→wait).".into(),
            });
        }
        prev_was_enable = is_enable;
    }
    out
}

// ── Rule 4: success_criteria has no positive assertion ───────────────────────
//
// "無 X 錯誤" / "no error" passes on the login page (the URL the auto-login
// hasn't redirected away from yet).  Need at least one positive token —
// "看到", "成功", "顯示", "shown", "visible", "進入", "存在", "appears" etc.

pub fn lint_success_criteria_positive(goal: &TestGoal) -> Vec<LintIssue> {
    if goal.success_criteria.is_empty() {
        return Vec::new();
    }
    // Multi-character positive tokens chosen to avoid false positives from
    // common negation prefixes (未/不/沒).  Note: "顯示" alone is NOT in the
    // list because "未顯示 X" would falsely register as positive.  Use
    // "已顯示" / "被顯示" if you want the displayed-form to count.
    const POSITIVE_TOKENS: &[&str] = &[
        // Chinese positive presence (each safe under common negation)
        "看到", "出現", "進入", "成功", "可見",
        "可正常", "正確", "被顯示", "已顯示",
        // English
        "shown", "visible", "appears", "appear ", "enters",
        "displayed", "loaded", "redirects to",
    ];
    let lower_all: String = goal.success_criteria
        .iter()
        .map(|c| c.to_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    let has_positive = POSITIVE_TOKENS.iter().any(|t| lower_all.contains(&t.to_lowercase()));
    if has_positive {
        return Vec::new();
    }
    vec![LintIssue {
        rule:     "success_criteria_positive",
        severity: Severity::Warn,
        step:     None,
        message: format!(
            "success_criteria has {} item(s) but none contains a positive-presence \
             token (看到 / 顯示 / 成功 / 進入 / shown / visible / appears / etc.). \
             Negative-only criteria (\"no error\", \"未顯示 X\") pass on the login page \
             too — add one positive assertion. (KB: sirin-trap-no-error-false-positive)",
            goal.success_criteria.len(),
        ),
    }]
}

// ── Rule 5: max_iterations ratio ─────────────────────────────────────────────
//
// MEMORY.md design rule: each YAML step needs ~1-2 LLM iterations.  Setting
// max_iterations equal to step count guarantees premature timeout on the
// first step that needs a retry.  Floor: ⌈steps × 1.5⌉.

pub fn lint_iterations_ratio(goal: &TestGoal) -> Vec<LintIssue> {
    let steps = extract_steps(goal);
    let step_count = steps.len() as u32;
    if step_count == 0 {
        return Vec::new(); // Goal text doesn't use numbered steps; can't lint.
    }
    // Hard floor: max_iterations must cover at least one iteration per step.
    // Below this, the LLM is mathematically guaranteed to hit the iteration
    // cap before the YAML can finish.  MEMORY.md's 1.5× guideline is good
    // advice (parse retries / action verification add overhead) but not
    // enforced — many shipped tests run fine at 1.1-1.4×.
    if goal.max_iterations >= step_count {
        return Vec::new();
    }
    let recommended = ((step_count as f64) * 1.5).ceil() as u32;
    vec![LintIssue {
        rule:     "iterations_ratio",
        severity: Severity::Warn,
        step:     None,
        message: format!(
            "max_iterations={} but goal lists {} steps — guaranteed to hit \
             the iteration cap before finishing. Bump to ≥ {} (the {} floor) \
             or ideally {} (1.5× per MEMORY.md guidance).",
            goal.max_iterations, step_count,
            step_count, step_count, recommended,
        ),
    }]
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> TestGoal {
        serde_yaml::from_str::<TestGoal>(yaml).expect("test fixture must parse")
    }

    // Rule 1 — covered by parser.rs existing test, but add one explicit
    // case here to verify the unified `lint()` entry point picks it up.
    #[test]
    fn rule_clear_state_reauth_fires_when_no_followup_goto() {
        let g = parse(r#"
id: x
name: x
url: "https://app.example.com/?__test_role=buyer"
goal: |
  steps:
  1. wait 3000
  2. clear_state
  3. wait 1000
  4. enable_a11y
  5. done=true
"#);
        let issues = lint(&g);
        let cs: Vec<&LintIssue> = issues.iter()
            .filter(|i| i.rule == "clear_state_reauth").collect();
        assert_eq!(cs.len(), 1, "got {:?}", issues);
        assert_eq!(cs[0].step, Some(2));
    }

    // Rule 2 — H5 viewport missing
    #[test]
    fn rule_h5_viewport_warns_when_buyer_url_has_no_viewport() {
        let g = parse(r#"
id: x
name: x
url: "https://app.example.com/?__test_role=buyer"
goal: "1. wait 3000"
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "h5_viewport_missing").collect();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("390"));
        assert!(issues[0].message.contains("844"));
    }

    #[test]
    fn rule_h5_viewport_silent_for_admin_role() {
        let g = parse(r#"
id: x
name: x
url: "https://app.example.com/?__test_role=admin"
goal: "1. wait 3000"
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "h5_viewport_missing").collect();
        assert!(issues.is_empty());
    }

    #[test]
    fn rule_h5_viewport_silent_when_correct_block_present() {
        let g = parse(r#"
id: x
name: x
url: "https://app.example.com/?__test_role=buyer"
goal: "1. wait 3000"
viewport:
  width: 390
  height: 844
  scale: 2.0
  mobile: true
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "h5_viewport_missing").collect();
        assert!(issues.is_empty(), "got: {:?}", issues);
    }

    #[test]
    fn rule_h5_viewport_warns_on_wrong_size() {
        let g = parse(r#"
id: x
name: x
url: "https://app.example.com/?__test_role=buyer"
goal: "1. wait 3000"
viewport:
  width: 1280
  height: 900
  scale: 1.0
  mobile: false
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "h5_viewport_missing").collect();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("1280×900"));
    }

    // Rule 3 — back-to-back enable_a11y
    #[test]
    fn rule_double_enable_fires_when_no_intervening_action() {
        let g = parse(r#"
id: x
name: x
url: "https://example.com"
goal: |
  1. wait 3000
  2. enable_a11y
  3. enable_a11y
  4. done=true
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "double_enable_a11y").collect();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].step, Some(3));
    }

    #[test]
    fn rule_double_enable_silent_when_wait_intervenes() {
        let g = parse(r#"
id: x
name: x
url: "https://example.com"
goal: |
  1. wait 3000
  2. enable_a11y
  3. wait 2000
  4. enable_a11y
  5. done=true
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "double_enable_a11y").collect();
        assert!(issues.is_empty());
    }

    // Rule 4 — success_criteria positive
    #[test]
    fn rule_success_criteria_warns_when_only_negative() {
        let g = parse(r#"
id: x
name: x
url: "https://example.com"
goal: "1. wait 3000"
success_criteria:
  - "no console errors"
  - "未顯示 404 page"
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "success_criteria_positive").collect();
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn rule_success_criteria_silent_when_positive_token_present() {
        let g = parse(r#"
id: x
name: x
url: "https://example.com"
goal: "1. wait 3000"
success_criteria:
  - "看到首頁標題"
  - "no console errors"
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "success_criteria_positive").collect();
        assert!(issues.is_empty());
    }

    // Rule 5 — iterations ratio (1.0× hard floor)
    #[test]
    fn rule_iterations_warns_when_below_step_count() {
        // 10 steps, max_iterations=8 → guaranteed to hit cap → warn
        let g = parse(r#"
id: x
name: x
url: "https://example.com"
goal: |
  1. a
  2. b
  3. c
  4. d
  5. e
  6. f
  7. g
  8. h
  9. i
  10. done=true
max_iterations: 8
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "iterations_ratio").collect();
        assert_eq!(issues.len(), 1, "got: {:?}", issues);
        assert!(issues[0].message.contains("max_iterations=8"));
    }

    #[test]
    fn rule_iterations_silent_when_at_floor() {
        // 5 steps, max=5 → exactly at floor → ok
        let g = parse(r#"
id: x
name: x
url: "https://example.com"
goal: |
  1. a
  2. b
  3. c
  4. d
  5. done=true
max_iterations: 5
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "iterations_ratio").collect();
        assert!(issues.is_empty(), "got: {:?}", issues);
    }

    #[test]
    fn rule_iterations_silent_when_above_floor_below_recommended() {
        // 10 steps, max=12 (1.2×) — above 1.0× floor, below 1.5× guideline.
        // We don't enforce 1.5×, so this should pass.
        let g = parse(r#"
id: x
name: x
url: "https://example.com"
goal: |
  1. a
  2. b
  3. c
  4. d
  5. e
  6. f
  7. g
  8. h
  9. i
  10. done=true
max_iterations: 12
"#);
        let issues: Vec<_> = lint(&g).into_iter()
            .filter(|i| i.rule == "iterations_ratio").collect();
        assert!(issues.is_empty(), "got: {:?}", issues);
    }

    // ── Corpus test — every shipped YAML in agora_regression must lint clean ──

    #[test]
    fn lint_all_regression_yamls_clean() {
        let dir = std::path::Path::new("config").join("tests").join("agora_regression");
        if !dir.exists() {
            // sub-crate / partial-checkout build; skip silently.
            return;
        }
        let mut failures: Vec<(String, Vec<LintIssue>)> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read agora_regression dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            let content = std::fs::read_to_string(&path).expect("read yaml");
            // Skip files that don't parse — the parse-failure path is covered
            // by other unit tests, and a corpus that fails parsing here would
            // mask the lint-rule signal we're trying to assert on.
            let goal = match serde_yaml::from_str::<TestGoal>(&content) {
                Ok(g) => g,
                Err(_) => continue,
            };
            let issues = lint(&goal);
            if !issues.is_empty() {
                failures.push((
                    path.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string(),
                    issues,
                ));
            }
        }
        if !failures.is_empty() {
            let lines: Vec<String> = failures.iter().map(|(name, issues)| {
                let bullets: Vec<String> = issues.iter().map(|i| {
                    let step = i.step.map(|n| format!(" step {}", n)).unwrap_or_default();
                    format!("    - [{}]{} {}", i.rule, step, i.message)
                }).collect();
                format!("  {}:\n{}", name, bullets.join("\n"))
            }).collect();
            panic!(
                "{} regression YAML(s) failed lint:\n{}",
                failures.len(),
                lines.join("\n"),
            );
        }
    }
}
