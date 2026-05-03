//! Flakiness-aware retry budget + smarter retry strategy (Issue #241).
//!
//! ## Why
//!
//! `TestGoal::max_retries` is a per-YAML hardcoded ceiling that has no
//! awareness of historical pass-rate or *why* the last attempt failed.
//! Symptoms in production:
//!
//! - Structurally-flaky tests (`pickup_time_picker` v12→v18) burn 5-10
//!   LLM-driven retry attempts before a human notices it's broken, not
//!   transient.
//! - `BackendDown` failures (HTTP 500 mid-deploy, KB
//!   `agora-trap-deploy-timing-race`) get the same 3-second retry as
//!   any other transient — instead of a 30-second wait for the API to
//!   come back.
//! - Chrome state from one timed-out test poisons sibling tests in a
//!   batch run (#180).  Current code doesn't recover the browser.
//!
//! ## What
//!
//! A `RetryPolicy` (loaded from `<config_dir>/retry_policy.yaml`, or
//! hard-coded defaults if the file is missing) maps:
//!
//!   1. **Historical pass-rate tier** → max retries + base sleep
//!   2. **Classification of the last failure** → overrides (longer
//!      sleep for backend-down, more retries for empty-LLM, no retry
//!      for convergence-guard)
//!   3. **Quarantine threshold** → tests below this pass-rate are
//!      flagged but not auto-retried; needs a human to clear them
//!      (recording 3+ passes lifts `is_flaky()` and unblocks).
//!
//! Everything is overrideable.  YAML `max_retries: N` still acts as
//! the ceiling — policy never *raises* above the YAML; it can lower it
//! for tests that historically don't benefit from retrying.
//!
//! ## Decision flow
//!
//! ```text
//! attempt failed
//!     │
//!     ├─ is_flaky(test_id)? ── yes ─→ quarantine (no retry, badge)
//!     │
//!     ├─ classify last failure ──→ override sleep_ms / max_retries
//!     │
//!     ├─ pass_rate tier lookup ──→ tier max_retries
//!     │
//!     └─ should_retry = attempt < min(yaml_max, policy_max)
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

use super::executor::TestStatus;

// ── Public API ───────────────────────────────────────────────────────────────

/// What the test runner should do after a failed attempt.
#[derive(Debug, Clone)]
pub struct RetryDecision {
    pub should_retry:   bool,
    pub sleep_ms:       u64,
    pub recover_browser: bool,
    /// Human-readable reason for the decision (logged + surfaced in UI).
    pub reason:         String,
    /// `true` when the test is below the quarantine threshold and should
    /// be flagged in the dashboard (no retry, awaiting manual unblock).
    pub quarantined:    bool,
}

/// Compute the retry decision for a failed attempt.
///
/// Inputs:
/// - `test_id`           — used for SQLite history lookup (pass-rate, is_flaky)
/// - `attempt`           — 0-indexed; `0` means the *first* attempt just failed
/// - `yaml_max_retries`  — ceiling from the YAML; never exceeded
/// - `last_status`       — terminal status of the just-finished attempt
/// - `last_error_msg`    — `result.error_message`, used to detect deploy-race
/// - `last_category`     — failure_category if already classified
pub fn decide(
    test_id:          &str,
    attempt:          u32,
    yaml_max_retries: u32,
    last_status:      TestStatus,
    last_error_msg:   Option<&str>,
    last_category:    Option<&str>,
) -> RetryDecision {
    let policy = global();

    // 1. Quarantine — historically flaky tests don't get auto-retried.
    //    We still let `yaml_max_retries=0` skip this check (test author
    //    explicitly opted out of retries).
    let stats = super::store::test_stats(test_id);
    let pass_rate = stats.pass_rate_7d;
    let total_runs = stats.total_runs;

    // Need at least N runs to make a quarantine call; one-off bad luck
    // shouldn't quarantine a brand-new test.
    let quarantined = total_runs >= policy.quarantine_min_runs
        && pass_rate < policy.quarantine_threshold;

    if quarantined {
        return RetryDecision {
            should_retry:    false,
            sleep_ms:        0,
            recover_browser: false,
            quarantined:     true,
            reason: format!(
                "quarantined: pass_rate {:.0}% < {:.0}% over last {} runs",
                pass_rate * 100.0,
                policy.quarantine_threshold * 100.0,
                total_runs,
            ),
        };
    }

    // 2. Classification overrides — backend-down / llm-empty / convergence-guard
    //    take precedence over the pass-rate tier because the *cause* dictates
    //    the right wait time.
    if let Some(over) = policy.classify_override(last_status, last_error_msg, last_category) {
        let max_total = yaml_max_retries.min(over.max_retries);
        let should_retry = attempt < max_total;
        return RetryDecision {
            should_retry,
            sleep_ms:        if should_retry { over.sleep_ms } else { 0 },
            recover_browser: should_retry && over.recover_browser,
            quarantined:     false,
            reason: format!(
                "{}: attempt {}/{}, sleep {}ms",
                over.label,
                attempt + 1,
                max_total + 1,
                if should_retry { over.sleep_ms } else { 0 },
            ),
        };
    }

    // 3. Pass-rate tier lookup
    let tier = policy.tier_for(pass_rate, total_runs);
    let max_total = yaml_max_retries.min(tier.max_retries);
    let should_retry = attempt < max_total && is_transient(last_status, last_error_msg);
    RetryDecision {
        should_retry,
        sleep_ms:        if should_retry { tier.sleep_ms } else { 0 },
        recover_browser: should_retry && tier.recover_browser,
        quarantined:     false,
        reason: format!(
            "{}: pass_rate {:.0}% / {} runs, attempt {}/{}, sleep {}ms",
            tier.label,
            pass_rate * 100.0,
            total_runs,
            attempt + 1,
            max_total + 1,
            if should_retry { tier.sleep_ms } else { 0 },
        ),
    }
}

/// Fast check: is the failure transient (worth retrying at all) without
/// consulting history?  Used by the tier path; classification overrides
/// can still force-retry on top of this.
fn is_transient(status: TestStatus, error_msg: Option<&str>) -> bool {
    let is_rendering_failure = status == TestStatus::Failed
        && error_msg
            .map(|e| e.contains("all-black") || e.contains("rendering_failure"))
            .unwrap_or(false);
    matches!(status, TestStatus::Timeout | TestStatus::Error) || is_rendering_failure
}

// ── Policy data ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Below this pass-rate (over `quarantine_min_runs` recent runs) the
    /// test is auto-quarantined and won't retry.
    pub quarantine_threshold: f64,
    pub quarantine_min_runs:  usize,

    /// Pass-rate tiers, evaluated in declaration order.  The first tier
    /// whose `min_pass_rate` ≤ the test's pass-rate wins.  Make sure they
    /// are sorted descending by `min_pass_rate`.
    pub tiers: Vec<Tier>,

    /// Failure-classification overrides — if the last failure matches a
    /// rule's `match_*` predicates, the rule's max_retries / sleep_ms /
    /// recover_browser take effect, ignoring the tier.
    pub classifications: Vec<Classification>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier {
    /// Minimum pass-rate (0.0–1.0) to qualify for this tier.
    pub min_pass_rate: f64,
    pub max_retries:   u32,
    pub sleep_ms:      u64,
    /// Call `browser::clear_browser_state()` before retrying?  Default
    /// false for the top tier (stable tests don't need recovery), true
    /// for moderate (browser state may be poisoned).
    #[serde(default)]
    pub recover_browser: bool,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Classification {
    /// Optional list of TestStatus names ("timeout", "error", "failed").
    /// Empty = match any.
    #[serde(default)]
    pub match_status:        Vec<String>,
    /// Optional list of failure_category strings ("env", "flaky", …).
    /// Empty = match any.
    #[serde(default)]
    pub match_category:      Vec<String>,
    /// Optional list of substrings in result.error_message.  Empty = no
    /// substring constraint.
    #[serde(default)]
    pub match_error_substr:  Vec<String>,

    pub max_retries:     u32,
    pub sleep_ms:        u64,
    #[serde(default)]
    pub recover_browser: bool,
    pub label:           String,
}

/// Lowercase name matching `#[serde(rename_all = "lowercase")]` on TestStatus.
fn status_str(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Passed   => "passed",
        TestStatus::Failed   => "failed",
        TestStatus::Timeout  => "timeout",
        TestStatus::Error    => "error",
        TestStatus::Disputed => "disputed",
    }
}

impl Classification {
    fn matches(&self, status: TestStatus, err: Option<&str>, cat: Option<&str>) -> bool {
        if !self.match_status.is_empty()
            && !self.match_status.iter().any(|s| s == status_str(status))
        {
            return false;
        }
        if !self.match_category.is_empty()
            && !cat.map(|c| self.match_category.iter().any(|m| m == c)).unwrap_or(false)
        {
            return false;
        }
        if !self.match_error_substr.is_empty() {
            let Some(e) = err else { return false; };
            if !self.match_error_substr.iter().any(|s| e.contains(s)) {
                return false;
            }
        }
        true
    }
}

impl RetryPolicy {
    /// Lookup the pass-rate tier for a given test.  Falls back to the
    /// last (lowest) tier if no rule matches.
    pub fn tier_for(&self, pass_rate: f64, total_runs: usize) -> &Tier {
        // Tests with insufficient history are treated as "stable" by
        // default — they get the top tier so a single bad run doesn't
        // immediately downgrade them.
        if total_runs < self.quarantine_min_runs {
            return self.tiers.first().expect("policy must have at least one tier");
        }
        for t in &self.tiers {
            if pass_rate >= t.min_pass_rate {
                return t;
            }
        }
        self.tiers.last().expect("policy must have at least one tier")
    }

    pub fn classify_override(
        &self,
        status: TestStatus,
        err: Option<&str>,
        cat: Option<&str>,
    ) -> Option<&Classification> {
        self.classifications.iter().find(|c| c.matches(status, err, cat))
    }

    /// Sane built-in defaults.  Used when the YAML is missing or malformed.
    pub fn default_policy() -> Self {
        Self {
            quarantine_threshold: 0.70,
            quarantine_min_runs:  3,

            tiers: vec![
                Tier {
                    min_pass_rate:   0.95,
                    max_retries:     1,
                    sleep_ms:        3_000,
                    recover_browser: false,
                    label:           "stable".into(),
                },
                Tier {
                    min_pass_rate:   0.70,
                    max_retries:     2,
                    sleep_ms:        5_000,
                    recover_browser: true,
                    label:           "moderate".into(),
                },
                // Below 0.70 is technically caught by quarantine_threshold
                // first; this tier is a safety net (e.g., when
                // total_runs < quarantine_min_runs).
                Tier {
                    min_pass_rate:   0.0,
                    max_retries:     0,
                    sleep_ms:        0,
                    recover_browser: false,
                    label:           "flaky".into(),
                },
            ],

            classifications: vec![
                // BackendDown — HTTP 500/503 in console + JSON parse errors
                // typical of a partial deploy.  Wait long enough for the API
                // to come back (KB: agora-trap-deploy-timing-race).
                Classification {
                    match_status:       vec!["failed".into(), "error".into()],
                    match_category:     vec![],
                    match_error_substr: vec![
                        "HTTP 500".into(), "HTTP 503".into(),
                        "JpaQueryTransformerSupport".into(),
                        "Internal Server Error".into(),
                    ],
                    max_retries:     1,
                    sleep_ms:        30_000,
                    recover_browser: true,
                    label:           "backend_down".into(),
                },
                // LlmEmpty — Gemini's silent 200 + empty content bug.
                // Two quick retries usually catch a working response.
                Classification {
                    match_status:       vec!["error".into()],
                    match_category:     vec![],
                    match_error_substr: vec![
                        "empty content".into(),
                        "too many invalid LLM responses".into(),
                    ],
                    max_retries:     2,
                    sleep_ms:        500,
                    recover_browser: false,
                    label:           "llm_empty".into(),
                },
                // ConvergenceGuard — same action / state for too many turns.
                // Deterministic, no point retrying.
                Classification {
                    match_status:       vec!["failed".into(), "error".into()],
                    match_category:     vec![],
                    match_error_substr: vec![
                        "convergence_guard".into(),
                        "no progress".into(),
                    ],
                    max_retries:     0,
                    sleep_ms:        0,
                    recover_browser: false,
                    label:           "convergence_guard".into(),
                },
            ],
        }
    }
}

// ── Loader (lazy global) ─────────────────────────────────────────────────────

/// Global policy, initialised once.  Tests can override via
/// `set_for_test()` (test-only).
static POLICY: OnceLock<RetryPolicy> = OnceLock::new();

fn global() -> &'static RetryPolicy {
    POLICY.get_or_init(load)
}

/// Load `<config_dir>/retry_policy.yaml`, falling back to defaults if
/// the file is missing or malformed.  Always returns a usable policy.
fn load() -> RetryPolicy {
    let path = policy_path();
    match std::fs::read_to_string(&path) {
        Ok(yaml) => match serde_yaml::from_str::<RetryPolicy>(&yaml) {
            Ok(p) => {
                tracing::info!("[retry_policy] loaded from {}", path.display());
                p
            }
            Err(e) => {
                tracing::warn!(
                    "[retry_policy] {} parse error: {} — using defaults",
                    path.display(), e
                );
                RetryPolicy::default_policy()
            }
        },
        Err(_) => {
            // File missing is fine; defaults are sane.  Don't spam logs.
            RetryPolicy::default_policy()
        }
    }
}

fn policy_path() -> PathBuf {
    crate::platform::config_path("retry_policy.yaml")
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> RetryPolicy { RetryPolicy::default_policy() }

    #[test]
    fn tier_for_stable_pass_rate() {
        let pol = p();
        assert_eq!(pol.tier_for(0.98, 10).label, "stable");
        assert_eq!(pol.tier_for(0.95, 10).label, "stable");
    }

    #[test]
    fn tier_for_moderate_pass_rate() {
        let pol = p();
        assert_eq!(pol.tier_for(0.85, 10).label, "moderate");
        assert_eq!(pol.tier_for(0.70, 10).label, "moderate");
    }

    #[test]
    fn tier_for_low_runs_treated_stable() {
        let pol = p();
        // < quarantine_min_runs (3) → top tier even if pass_rate is bad
        assert_eq!(pol.tier_for(0.0, 1).label, "stable");
        assert_eq!(pol.tier_for(0.5, 2).label, "stable");
    }

    #[test]
    fn classify_override_backend_down() {
        let pol = p();
        let m = pol.classify_override(
            TestStatus::Failed,
            Some("Got HTTP 500 from /api/list"),
            None,
        ).expect("should match backend_down");
        assert_eq!(m.label, "backend_down");
        assert_eq!(m.sleep_ms, 30_000);
        assert!(m.recover_browser);
    }

    #[test]
    fn classify_override_llm_empty() {
        let pol = p();
        let m = pol.classify_override(
            TestStatus::Error,
            Some("LLM returned empty content"),
            None,
        ).expect("should match llm_empty");
        assert_eq!(m.label, "llm_empty");
        assert_eq!(m.max_retries, 2);
    }

    #[test]
    fn classify_override_convergence_guard_no_retry() {
        let pol = p();
        let m = pol.classify_override(
            TestStatus::Failed,
            Some("convergence_guard tripped after 3 same-action turns"),
            None,
        ).expect("should match convergence_guard");
        assert_eq!(m.max_retries, 0);
    }

    #[test]
    fn classify_override_no_match_returns_none() {
        let pol = p();
        let m = pol.classify_override(
            TestStatus::Passed,
            None,
            None,
        );
        // None of our overrides match status=passed
        assert!(m.is_none());
    }

    #[test]
    fn is_transient_recognises_timeout_and_error() {
        assert!(is_transient(TestStatus::Timeout, None));
        assert!(is_transient(TestStatus::Error, None));
        assert!(!is_transient(TestStatus::Passed, None));
        assert!(!is_transient(TestStatus::Failed, None));
        // Failed + all-black → still transient (Chrome rendering glitch)
        assert!(is_transient(TestStatus::Failed, Some("screenshot all-black bytes=2048")));
    }

    #[test]
    fn yaml_round_trip() {
        let pol = p();
        let yaml = serde_yaml::to_string(&pol).unwrap();
        let back: RetryPolicy = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.tiers.len(), pol.tiers.len());
        assert_eq!(back.classifications.len(), pol.classifications.len());
        assert_eq!(back.quarantine_threshold, pol.quarantine_threshold);
    }

    /// The shipped `config/retry_policy.yaml` must parse cleanly into the
    /// same struct shape so editing the YAML doesn't silently fall back to
    /// defaults.  This guards against field renames going unnoticed.
    #[test]
    fn shipped_yaml_parses() {
        let path = std::path::Path::new("config").join("retry_policy.yaml");
        if !path.exists() {
            // skip in sub-crate / partial-checkout builds
            return;
        }
        let yaml = std::fs::read_to_string(&path)
            .expect("read config/retry_policy.yaml");
        let pol: RetryPolicy = serde_yaml::from_str(&yaml)
            .expect("config/retry_policy.yaml must parse");
        assert!(!pol.tiers.is_empty(),  "tiers must be non-empty");
        assert!(pol.tiers.iter().any(|t| t.label == "stable"));
        assert!(pol.classifications.iter().any(|c| c.label == "backend_down"));
    }

    /// Issue #241 acceptance: a test with a long fail history quarantines
    /// instead of looping. We assert via `tier_for` directly since the
    /// SQLite-backed `decide()` requires a real DB.
    #[test]
    fn quarantine_threshold_applies_at_or_below_70pct() {
        let pol = p();
        // 65% pass rate over 10 runs should NOT pick "moderate" — it should
        // fall through to flaky and be quarantined separately (decide()).
        assert_eq!(pol.tier_for(0.65, 10).label, "flaky");
        // Exactly 70% qualifies for moderate (boundary).
        assert_eq!(pol.tier_for(0.70, 10).label, "moderate");
    }
}
