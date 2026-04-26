/// URL Blocker — pre-navigation guard against blocked URL patterns.
///
/// Tracking issue: <https://github.com/Redandan/Sirin/issues/81>
///
/// # Why a separate layer
///
/// The existing `authz::engine` `deny` rule list is already capable of
/// blocking URLs by glob pattern.  However it is consulted only when
/// `decide()` is called from `mcp_server::call_browser_exec` — which means:
///   * an LLM has to reason its way to a `web_navigate { action: goto, … }`
///     call before the rule fires
///   * tokens are spent producing a request that will be rejected
///
/// CiC's managed-policy `blockedUrlPatterns` runs at
/// `chrome.webNavigation.onBeforeNavigate` — i.e. *before* any navigation
/// turns into a request.  We mirror that here at the `web_navigate` /
/// `goto` boundary in [`crate::adk::tool::builtins`] so the pattern check
/// happens before the browser ever sees the URL.
///
/// # Pattern syntax
///
/// Powered by [`globset`] (already a dependency).  Patterns use:
///   * `*`  — matches any characters except `/` is **not** a separator
///     here (we set `literal_separator(false)` so URLs match naturally)
///   * `**` — matches any path including separators
///   * exact strings match exactly
///
/// Examples:
///   * `https://*.bank.com/**`
///   * `**/admin/payment/confirm*`
///   * `https://github.com/*/settings/billing`
use globset::{Glob, GlobBuilder, GlobMatcher};

/// Compiled pattern + the original source string (for error messages).
struct Compiled {
    source: String,
    matcher: GlobMatcher,
}

/// A list of compiled glob patterns used to block URLs.
pub struct UrlBlocker {
    patterns: Vec<Compiled>,
}

#[allow(dead_code)] // Public API; consumers (MCP `list_blocked_patterns`, future UI) wire in incrementally.
impl UrlBlocker {
    /// Compile a list of glob patterns into a blocker.
    ///
    /// Invalid patterns are silently skipped (logged at `warn`).  This
    /// fails open for the policy author but never panics on a typo —
    /// matching the philosophy of `engine::glob_matches`.
    pub fn from_patterns<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut compiled = Vec::new();
        for p in patterns {
            let src = p.as_ref();
            match build_glob(src) {
                Ok(g) => compiled.push(Compiled {
                    source: src.to_string(),
                    matcher: g.compile_matcher(),
                }),
                Err(e) => {
                    tracing::warn!(
                        "[authz::url_blocker] skipping invalid pattern {src:?}: {e}"
                    );
                }
            }
        }
        Self { patterns: compiled }
    }

    /// Returns the pattern string that matched, or `None` if URL is allowed.
    pub fn check<'a>(&'a self, url: &str) -> Option<&'a str> {
        self.patterns
            .iter()
            .find(|p| p.matcher.is_match(url))
            .map(|p| p.source.as_str())
    }

    /// Convenience: bool form of [`Self::check`].
    pub fn is_blocked(&self, url: &str) -> bool {
        self.check(url).is_some()
    }

    /// Number of compiled patterns.  Mostly for diagnostics / `list_blocked_patterns`.
    pub fn len(&self) -> usize { self.patterns.len() }

    /// `true` if no patterns were compiled (e.g. blocklist disabled).
    pub fn is_empty(&self) -> bool { self.patterns.is_empty() }

    /// Iterate the original pattern strings — for diagnostics / MCP exposure.
    pub fn sources(&self) -> impl Iterator<Item = &str> {
        self.patterns.iter().map(|p| p.source.as_str())
    }
}

/// Build a glob with the same options the rest of the authz stack uses.
fn build_glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern)
        .case_insensitive(false)
        .literal_separator(false) // `*` may span URL path segments
        .build()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod url_blocker_test {
    use super::*;

    #[test]
    fn blocks_exact_glob() {
        let b = UrlBlocker::from_patterns(["https://*.bank.com/**"]);
        assert!(b.is_blocked("https://login.bank.com/account"));
        assert!(b.is_blocked("https://www.bank.com/transfer"));
    }

    #[test]
    fn allows_when_no_pattern_matches() {
        let b = UrlBlocker::from_patterns(["**/admin/**"]);
        assert!(!b.is_blocked("https://example.com/home"));
        assert!(!b.is_blocked("https://github.com/foo/bar"));
    }

    #[test]
    fn multiple_patterns_returns_first_match() {
        let b = UrlBlocker::from_patterns([
            "https://*.evil.com/**",
            "**/admin/payment/confirm*",
            "https://prod.*.example.com/**",
        ]);
        // Path-style match
        let hit = b.check("https://api.example.org/admin/payment/confirm?id=1");
        assert_eq!(hit, Some("**/admin/payment/confirm*"));
        // Subdomain match
        let hit2 = b.check("https://attacker.evil.com/x");
        assert_eq!(hit2, Some("https://*.evil.com/**"));
        // Non-match
        assert!(!b.is_blocked("https://safe.example.com/"));
    }

    #[test]
    fn empty_blocker_allows_everything() {
        let b = UrlBlocker::from_patterns::<_, &str>(std::iter::empty::<&str>());
        assert!(b.is_empty());
        assert!(!b.is_blocked("https://anything.example/"));
    }

    #[test]
    fn invalid_pattern_is_skipped_not_fatal() {
        // `[` without close is invalid.  Constructor must not panic.
        let b = UrlBlocker::from_patterns(["[broken", "https://valid.com/**"]);
        assert_eq!(b.len(), 1);
        assert!(b.is_blocked("https://valid.com/path"));
    }

    #[test]
    fn double_star_matches_path_with_separators() {
        let b = UrlBlocker::from_patterns(["https://github.com/*/settings/billing"]);
        assert!(b.is_blocked("https://github.com/myorg/settings/billing"));
        assert!(!b.is_blocked("https://github.com/myorg/repo/settings/billing"));
    }
}
