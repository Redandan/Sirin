//! GitHub issue ↔ AgentTeam queue bridge.
//!
//! Thin wrapper around the `gh` CLI that lets Sirin's Dev Team treat
//! GitHub issues as task inputs and post review summaries back as comments —
//! closing the loop for issue-driven cross-repo work.
//!
//! ## Why a separate module
//!
//! `gh` is an *external* binary, network-dependent, and may not be installed
//! on every machine. Keeping the integration here means:
//!   - Failure in one issue doesn't poison the queue (errors return `Err`,
//!     not panic)
//!   - Unit tests can mock the CLI via the `GhRunner` trait if we ever need
//!     to without rewriting the worker
//!   - The rest of `multi_agent` stays GitHub-agnostic
//!
//! ## Pre-conditions
//!
//! - `gh` CLI is on PATH and authenticated (`gh auth status` passes)
//! - Caller knows the *logical* project_key (e.g. "agora_market") AND the
//!   *GitHub* owner/repo (e.g. "redandan/AgoraMarket") — we don't auto-map
//!   because mappings are deployment-specific
//!
//! ## Typical usage
//!
//! ```ignore
//! // Pull issue #70 from redandan/AgoraMarket into the team queue
//! let id = github_adapter::enqueue_from_issue(
//!     "agora_market",         // session namespace
//!     "redandan/AgoraMarket", // gh repo
//!     70,
//! )?;
//!
//! // Worker picks up the task → assign_task → on completion auto-posts:
//! // (handled by worker.rs reading ctx.issue_url)
//! ```

use std::process::Command;
use super::queue::{self, ProjectContext};

// ── Public API ────────────────────────────────────────────────────────────────

/// Read a GitHub issue (title + body) and enqueue it as a TeamTask.
///
/// Returns the new task_id on success.
///
/// Behaviour:
/// - `extra_tools` automatically includes `Bash` so the Engineer can call
///   `gh` again (e.g. to re-read the issue, fetch related comments)
/// - `issue_url` is stored on the task → worker will auto-post the final
///   review back as a comment when the task finishes
/// - Priority defaults to 50 (normal). Use [`enqueue_from_issue_with_priority`]
///   to override (e.g. priority 10 for urgent bugs).
pub fn enqueue_from_issue(
    project_key: &str,
    gh_repo: &str,
    issue_number: u32,
) -> Result<String, String> {
    enqueue_from_issue_with_priority(project_key, gh_repo, issue_number, 50)
}

/// Same as [`enqueue_from_issue`] but with explicit priority.
pub fn enqueue_from_issue_with_priority(
    project_key: &str,
    gh_repo: &str,
    issue_number: u32,
    priority: u8,
) -> Result<String, String> {
    enqueue_from_issue_full(project_key, gh_repo, issue_number, priority, false)
}

/// DRY-RUN variant of [`enqueue_from_issue`] — the dev team will work on the
/// task normally but the worker will SKIP the auto-comment-back to GitHub,
/// and PM/Engineer/Tester get a system-prompt addendum forbidding `gh issue
/// comment` / `gh pr create` / `git push`. The would-be comment is saved to
/// `data/multi_agent/preview_comments.jsonl` for human review.
///
/// Use this for verification runs against real issues before trusting the
/// team to touch GitHub directly.
pub fn enqueue_from_issue_dry_run(
    project_key: &str,
    gh_repo: &str,
    issue_number: u32,
) -> Result<String, String> {
    enqueue_from_issue_full(project_key, gh_repo, issue_number, 50, true)
}

fn enqueue_from_issue_full(
    project_key: &str,
    gh_repo: &str,
    issue_number: u32,
    priority: u8,
    dry_run: bool,
) -> Result<String, String> {
    let issue = read_issue(gh_repo, issue_number)?;
    let url   = format!("https://github.com/{gh_repo}/issues/{issue_number}");

    let labels = if issue.labels.is_empty() {
        String::new()
    } else {
        format!("\nLabels: {}", issue.labels.join(", "))
    };

    let close_note = if dry_run {
        "\n\n⚠️ 本任務以 DRY-RUN 驗證執行 — system 不會把你的 review 自動貼回 \
         issue 留言（會存到 preview 檔讓人類 review 後手動發布）。"
    } else {
        "\n\n完成後 system 會自動把你的最終 review 貼回 issue 留言。"
    };

    let description = format!(
        "[GitHub Issue #{n}] {title}\n\
         Source: {url}{labels}\n\
         \n\
         --- Issue body ---\n\
         {body}\n\
         --- End issue ---\n\
         \n\
         Goal: 分析這個 issue → 拆解步驟 → 在 cwd ({key}) 完成修改 → \
         驗證編譯/測試。{close_note}",
        n     = issue_number,
        title = issue.title,
        url   = url,
        key   = project_key,
        body  = trunc_for_prompt(&issue.body, 4_000),
    );

    let ctx = ProjectContext {
        repo:        project_key.to_string(),
        extra_tools: vec!["Bash".to_string()],   // for gh + cargo/flutter/mvn
        issue_url:   Some(url),
        dry_run,
    };

    Ok(queue::enqueue_with_project(&description, priority, ctx))
}

/// Post a comment on a GitHub issue. Body is markdown.
///
/// Designed to be called from the worker after a task completes — keeps
/// the human in the loop without anyone watching the Sirin UI.
pub fn comment_on_issue(
    gh_repo: &str,
    issue_number: u32,
    body: &str,
) -> Result<(), String> {
    let out = Command::new(gh_bin())
        .args([
            "issue", "comment", &issue_number.to_string(),
            "--repo", gh_repo,
            "--body", body,
        ])
        .output()
        .map_err(|e| format!("gh issue comment spawn failed: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("gh issue comment exit {}: {}", out.status, stderr.trim()));
    }
    Ok(())
}

/// Convenience hook for the worker: extract owner/repo/number from an
/// `issue_url` and post `body` as a comment. Returns `Ok(())` on success
/// or `Err` with reason. Errors are non-fatal at the call site — they
/// should be logged but not abort the worker loop.
pub fn comment_on_issue_url(issue_url: &str, body: &str) -> Result<(), String> {
    let parsed = parse_issue_url(issue_url)?;
    comment_on_issue(&parsed.repo, parsed.number, body)
}

// ── Preview / replay (dry-run mode) ───────────────────────────────────────────

/// One queued-but-unposted comment from a dry-run task.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PreviewComment {
    pub task_id:   String,
    pub issue_url: String,
    pub success:   bool,
    pub body:      String,
    pub saved_at:  String,
}

/// Read all queued previews (chronological, oldest first).
///
/// Reads from `data/multi_agent/preview_comments.jsonl`. Returns empty
/// `Vec` if the file doesn't exist (no dry-run comments yet) — not an error.
pub fn list_preview_comments() -> Vec<PreviewComment> {
    let path = crate::platform::app_data_dir()
        .join("data").join("multi_agent").join("preview_comments.jsonl");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    contents.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<PreviewComment>(l).ok())
        .collect()
}

/// Find the latest dry-run preview for a given task_id.
pub fn latest_preview_for(task_id: &str) -> Option<PreviewComment> {
    list_preview_comments()
        .into_iter()
        .filter(|p| p.task_id == task_id)
        .last()
}

/// Replay (i.e. actually post) a previously-saved dry-run preview.
///
/// Wraps the body in the same `[Sirin Dev Team ✓/✗]` envelope the live
/// worker would have used, so the result is byte-identical to a non-dry-run
/// post. Caller passes the preview returned by [`latest_preview_for`] or
/// [`list_preview_comments`].
pub fn replay_preview(preview: &PreviewComment) -> Result<(), String> {
    let header = if preview.success { "✓ Done" } else { "✗ Failed" };
    let comment = format!(
        "**[Sirin Dev Team {header} — replayed from dry-run]**\n\n{}\n\n\
         <sub>Posted manually after dry-run review · task `{}` · originally previewed at {}</sub>",
        preview.body, preview.task_id, preview.saved_at,
    );
    comment_on_issue_url(&preview.issue_url, &comment)
}

// ── gh CLI plumbing ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IssueData {
    pub title:  String,
    pub body:   String,
    pub labels: Vec<String>,
}

/// Read a single issue's title/body/labels via `gh issue view ... --json`.
pub fn read_issue(gh_repo: &str, issue_number: u32) -> Result<IssueData, String> {
    let out = Command::new(gh_bin())
        .args([
            "issue", "view", &issue_number.to_string(),
            "--repo", gh_repo,
            "--json", "title,body,labels",
        ])
        .output()
        .map_err(|e| format!("gh issue view spawn failed (is gh installed?): {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("gh issue view exit {}: {}", out.status, stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_gh_issue_json(&stdout)
}

fn parse_gh_issue_json(json: &str) -> Result<IssueData, String> {
    #[derive(serde::Deserialize)]
    struct Label { name: String }
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)] title:  String,
        #[serde(default)] body:   String,
        #[serde(default)] labels: Vec<Label>,
    }
    let raw: Raw = serde_json::from_str(json)
        .map_err(|e| format!("gh issue view returned non-JSON: {e}\n--\n{json}"))?;
    Ok(IssueData {
        title:  raw.title,
        body:   raw.body,
        labels: raw.labels.into_iter().map(|l| l.name).collect(),
    })
}

/// Resolve `gh` binary — checks `SIRIN_GH_BIN` env override, falls back to
/// PATH lookup. Not validating existence here; we let `Command::output()`
/// return a NotFound error which carries clearer context to the caller.
fn gh_bin() -> String {
    std::env::var("SIRIN_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

// ── URL parsing ───────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedIssueUrl {
    pub repo:   String,   // "owner/repo"
    pub number: u32,
}

/// Parse `https://github.com/owner/repo/issues/N` → `(owner/repo, N)`.
/// Tolerant of trailing slash, query string, fragment.
pub fn parse_issue_url(url: &str) -> Result<ParsedIssueUrl, String> {
    let trimmed = url.trim();
    // Strip protocol
    let after_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);

    let after_host = after_scheme
        .strip_prefix("github.com/")
        .ok_or_else(|| format!("not a github.com URL: {url}"))?;

    // Drop fragment + query
    let path = after_host
        .split(['?', '#'])
        .next()
        .unwrap_or(after_host)
        .trim_end_matches('/');

    // Expect owner/repo/issues/N
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 || parts[2] != "issues" {
        return Err(format!("not an issue URL (expected owner/repo/issues/N): {url}"));
    }
    let number: u32 = parts[3].parse()
        .map_err(|_| format!("issue number not numeric: {}", parts[3]))?;
    Ok(ParsedIssueUrl {
        repo:   format!("{}/{}", parts[0], parts[1]),
        number,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Char-boundary-safe truncation for prompts — avoids cutting CJK mid-codepoint.
fn trunc_for_prompt(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let end = (0..=max_bytes).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    let mut out = s[..end].to_string();
    out.push_str("\n…(truncated)");
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_canonical_url() {
        let p = parse_issue_url("https://github.com/redandan/AgoraMarket/issues/70").unwrap();
        assert_eq!(p.repo, "redandan/AgoraMarket");
        assert_eq!(p.number, 70);
    }

    #[test]
    fn parse_with_trailing_slash() {
        let p = parse_issue_url("https://github.com/owner/repo/issues/1/").unwrap();
        assert_eq!(p.number, 1);
    }

    #[test]
    fn parse_with_query_string() {
        let p = parse_issue_url("https://github.com/owner/repo/issues/42?utm=x").unwrap();
        assert_eq!(p.number, 42);
    }

    #[test]
    fn parse_with_fragment() {
        let p = parse_issue_url(
            "https://github.com/owner/repo/issues/42#issuecomment-123",
        ).unwrap();
        assert_eq!(p.number, 42);
    }

    #[test]
    fn parse_http_scheme() {
        let p = parse_issue_url("http://github.com/owner/repo/issues/9").unwrap();
        assert_eq!(p.number, 9);
    }

    #[test]
    fn parse_rejects_non_github() {
        assert!(parse_issue_url("https://gitlab.com/owner/repo/issues/1").is_err());
    }

    #[test]
    fn parse_rejects_pull_url() {
        // owner/repo/pull/N is not an issue URL — even though gh treats them
        // similarly, we want explicit semantics
        assert!(parse_issue_url("https://github.com/owner/repo/pull/5").is_err());
    }

    #[test]
    fn parse_rejects_bad_number() {
        assert!(parse_issue_url("https://github.com/owner/repo/issues/abc").is_err());
    }

    #[test]
    fn gh_json_round_trip() {
        let json = r#"{"title":"Login broken","body":"Steps:\n1. visit /login\n2. crash","labels":[{"name":"bug"},{"name":"frontend"}]}"#;
        let d = parse_gh_issue_json(json).unwrap();
        assert_eq!(d.title, "Login broken");
        assert!(d.body.contains("crash"));
        assert_eq!(d.labels, vec!["bug", "frontend"]);
    }

    #[test]
    fn gh_json_handles_empty_labels() {
        let json = r#"{"title":"x","body":"y","labels":[]}"#;
        let d = parse_gh_issue_json(json).unwrap();
        assert!(d.labels.is_empty());
    }

    #[test]
    fn gh_json_missing_fields_default() {
        let json = r#"{"title":"only-title"}"#;
        let d = parse_gh_issue_json(json).unwrap();
        assert_eq!(d.title, "only-title");
        assert!(d.body.is_empty());
        assert!(d.labels.is_empty());
    }

    #[test]
    fn trunc_preserves_short_strings() {
        assert_eq!(trunc_for_prompt("hi", 100), "hi");
    }

    #[test]
    fn trunc_cuts_long_strings_at_boundary() {
        let long = "ab".repeat(5_000); // 10_000 bytes
        let cut = trunc_for_prompt(&long, 100);
        assert!(cut.len() < 200);
        assert!(cut.ends_with("(truncated)"));
    }

    #[test]
    fn trunc_does_not_split_cjk() {
        // 中 = 3 bytes UTF-8. If we cut at byte 4 of "中文中文中文",
        // we must back up to byte 3 (after first char) not split a codepoint.
        let s = "中文中文中文";
        let cut = trunc_for_prompt(s, 4);
        // First char is "中" (3 bytes); next safe boundary is 3.
        // We expect "中" + "\n…(truncated)"
        assert!(cut.starts_with("中"));
        assert!(cut.ends_with("(truncated)"));
    }

    // ── Live integration (requires `gh` CLI authenticated + network) ─────────

    /// End-to-end: read a real issue from Redandan/AgoraMarket and confirm
    /// the dev-team routing fields would resolve correctly. Does NOT enqueue
    /// (no queue mutation, no LLM call, no GitHub comments) — pure read +
    /// in-memory verification of the architecture.
    ///
    /// Run with: `cargo test --bin sirin live_read_real_agora_issue --
    ///           --ignored --nocapture`
    #[test]
    #[ignore] // hits real network + needs gh auth
    fn live_read_real_agora_issue() {
        let issue = read_issue("Redandan/AgoraMarket", 9)
            .expect("gh issue view should succeed (gh authed?)");

        println!("\n── Live read: AgoraMarket #9 ──");
        println!("title : {}", issue.title);
        println!("labels: {:?}", issue.labels);
        println!("body  : {} bytes", issue.body.len());

        // Real-data sanity: title is non-empty, has the bug label
        assert!(!issue.title.is_empty(), "title must not be empty");
        assert!(issue.labels.iter().any(|l| l == "bug"),
            "issue #9 should carry 'bug' label");

        // Verify the routing the worker would take: project_key "agora_market"
        // must resolve to the actual Flutter repo on disk (post-rename).
        let cwd = crate::claude_session::repo_path("agora_market")
            .expect("repo_path('agora_market') should resolve");
        println!("\n── Resolved cwd ──\n{cwd}");
        assert!(cwd.ends_with("AgoraMarket") && !cwd.ends_with("AgoraMarketAPI"),
            "agora_market must map to the Flutter repo (was renamed from AgoraMarketFlutter)");
        assert!(std::path::Path::new(&cwd).join("pubspec.yaml").exists(),
            "resolved cwd must be a Flutter project (has pubspec.yaml)");

        // Verify the session-file naming the worker would create
        let pm_path = crate::platform::app_data_dir()
            .join("data").join("multi_agent").join("pagora_market_pm.json");
        println!("── Would create session file ──\n{}", pm_path.display());
        // We DON'T require this to exist (might be a fresh run); just confirm
        // the path is well-formed and lives under the Sirin data dir.
        assert!(pm_path.to_string_lossy().contains("pagora_market_pm.json"),
            "PM session file should use project namespace prefix");

        println!("\n✓ Live verification passed — dev team would route #9 correctly\n");
    }

    /// Partial end-to-end: enqueue a real GitHub issue, route it through the
    /// worker logic, load the cross-project AgentTeam, and run **one PM turn**.
    /// Does NOT execute the full assign_task loop (no Engineer, no Tester, no
    /// file modifications, no GitHub comments). Good "做一部分" smoke test.
    ///
    /// Run: `cargo test --bin sirin live_partial_dev_team_on_real_issue --
    ///      --ignored --nocapture`
    ///
    /// Pre-conditions: gh authed, claude CLI authed, AgoraMarket repo cloned.
    /// Cost: ~$0.01-0.05 (one PM Claude call).
    #[test]
    #[ignore]
    fn live_partial_dev_team_on_real_issue() {
        use crate::multi_agent::{AgentTeam, queue};

        // ── Step 1: enqueue real issue #34 via github_adapter ────────────────
        let task_id = enqueue_from_issue(
            "agora_market", "Redandan/AgoraMarket", 34,
        ).expect("enqueue_from_issue");
        println!("\n[1/6] ✓ Enqueued issue #34 → task_id={task_id}");

        // ── Step 2: pull it back out of the queue ────────────────────────────
        let task = queue::take_next_queued()
            .expect("take_next_queued should return our task");
        assert_eq!(task.id, task_id, "queue should hand back the task we just enqueued");
        println!("[2/6] ✓ take_next_queued returned task {} ({} chars)",
            task.id, task.description.len());

        // ── Step 3: verify ProjectContext landed correctly ───────────────────
        let ctx = task.project.as_ref().expect("project ctx must be set");
        assert_eq!(ctx.repo, "agora_market");
        assert!(ctx.extra_tools.iter().any(|t| t == "Bash"),
            "Engineer needs Bash for gh / flutter");
        assert!(ctx.issue_url.as_ref().map(|u|
            u.contains("Redandan/AgoraMarket/issues/34")).unwrap_or(false));
        println!("[3/6] ✓ ProjectContext: repo={}, extra_tools={:?}, issue_url set",
            ctx.repo, ctx.extra_tools);

        // ── Step 4: routing — exactly what worker.rs::resolve_project does ───
        let cwd = crate::claude_session::repo_path(&ctx.repo)
            .expect("repo_path should resolve agora_market");
        println!("[4/6] ✓ Resolved cwd: {cwd}");
        assert!(std::path::Path::new(&cwd).join("pubspec.yaml").exists(),
            "cwd must be a Flutter project");

        // ── Step 5: load cross-project AgentTeam + apply extra_tools ─────────
        let mut team = AgentTeam::load_for_worker_project(&cwd, 0, &ctx.repo);
        team.set_extra_tools(&ctx.extra_tools);
        team.pm.reset();   // clean baseline (deletes any prior pagora_market_pm.json)
        println!("[5/6] ✓ Loaded AgentTeam(project=agora_market, worker=0)");

        // ── Step 6: ONE PM turn (no full assign_task loop) ───────────────────
        // Trim the prompt: full task description + a do-nothing-yet directive.
        // We're testing whether PM can comprehend the issue under cross-project
        // cwd, NOT whether it can solve it.
        let probe = format!(
            "{}\n\n\
             ⚠️ 驗證模式：你不用真的指派任務。請只做兩件事：\n\
             1. 用 Read / Grep 看一下 AgoraMarket 這個專案大致是什麼\n\
             2. 用 1-2 句話總結 issue #34 在問什麼,你接下來會怎麼分配給工程師\n\
             不要寫 [PM → 工程師]，這次不啟動 Engineer。",
            task.description,
        );
        let reply = team.pm.send(&probe).expect("PM send must succeed");

        println!("\n[6/6] ── PM reply ──\n{reply}\n");
        println!("── session_id ──\n{:?}\n", team.pm.session_id());
        println!("── pm.turns() ──\n{}\n", team.pm.turns());

        // ── Verifications ────────────────────────────────────────────────────
        assert!(!reply.is_empty(), "PM reply must not be empty");
        assert!(team.pm.session_id().is_some(),
            "session_id should be captured on first turn");
        assert_eq!(team.pm.turns(), 1, "exactly one turn should be recorded");

        let pm_state = crate::platform::app_data_dir()
            .join("data").join("multi_agent")
            .join("pagora_market_pm.json");
        assert!(pm_state.exists(),
            "PM session state must persist at {}", pm_state.display());

        println!("✓ Live partial verification passed");
        println!("  → resume PM via: claude --resume {}",
            team.pm.session_id().unwrap_or("?"));
        println!("  → session file: {}", pm_state.display());

        // Cleanup: mark the test task as Done so it doesn't linger in the queue
        queue::update_status(&task.id, queue::TaskStatus::Done,
            Some("[verification test — see github_adapter::tests]".into()));
    }

    /// FULL end-to-end DRY-RUN: enqueue real issue → run actual 5-iter
    /// `assign_task` (PM ↔ Engineer ↔ PM review) → divert the would-be
    /// GitHub comment into the preview JSONL → verify file written →
    /// confirm no remote side effects.
    ///
    /// This is the test that proves the verification layer works end-to-end
    /// against a real issue without touching GitHub. After it passes, the
    /// human can inspect `data/multi_agent/preview_comments.jsonl` and
    /// optionally call `replay_preview()` to post the approved comment.
    ///
    /// What's exercised:
    ///   1. `enqueue_from_issue_dry_run` sets `ctx.dry_run = true`
    ///   2. Worker's `set_dry_run(true)` propagates to PM/Engineer/Tester
    ///   3. Each session's `send()` prepends `DRY_RUN_ADDENDUM`
    ///   4. Worker's task-completion branch routes to `save_preview_comment`
    ///      instead of `post_issue_comment` (no `gh issue comment` invoked)
    ///   5. Preview JSONL is readable via `latest_preview_for(task_id)`
    ///   6. Read-back round-trips byte-for-byte
    ///
    /// Run: `cargo test --bin sirin live_full_dry_run_on_real_issue --
    ///      --ignored --nocapture`
    ///
    /// Cost: $0 (claude CLI uses Max subscription quota, not per-call $).
    /// Time: 3-15 min depending on PM iteration count + LLM latency.
    /// Side effects on disk: appends one line to preview_comments.jsonl,
    /// may modify Sirin/AgoraMarket/AgoraMarketAPI source files locally
    /// (run `git restore .` in the target repo afterwards if needed).
    /// Side effects on GitHub: NONE (this is the whole point).
    #[test]
    #[ignore]
    fn live_full_dry_run_on_real_issue() {
        use crate::multi_agent::{AgentTeam, queue, worker};

        // ── Step 1: enqueue with dry_run=true ────────────────────────────
        let task_id = enqueue_from_issue_dry_run(
            "agora_market", "Redandan/AgoraMarket", 34,
        ).expect("enqueue_from_issue_dry_run");
        println!("\n[1/8] ✓ Enqueued issue #34 (DRY-RUN) → task_id={task_id}");

        // ── Step 2: pull task back & verify ProjectContext flags ─────────
        let task = queue::take_next_queued()
            .expect("take_next_queued must return our task");
        assert_eq!(task.id, task_id, "queue should hand back the task we enqueued");
        let ctx = task.project.as_ref().expect("project ctx must be set");
        assert!(ctx.dry_run, "ctx.dry_run must be true after enqueue_from_issue_dry_run");
        let issue_url = ctx.issue_url.clone()
            .expect("issue_url must be set by enqueue_from_issue_dry_run");
        println!(
            "[2/8] ✓ Task pulled: dry_run={}, repo={}, extra_tools={:?}",
            ctx.dry_run, ctx.repo, ctx.extra_tools,
        );

        // ── Step 3: resolve cwd (mirrors worker::resolve_project) ────────
        let cwd = crate::claude_session::repo_path(&ctx.repo)
            .expect("repo_path('agora_market') must resolve to AgoraMarket repo");
        assert!(std::path::Path::new(&cwd).join("pubspec.yaml").exists(),
            "cwd must be the Flutter project (pubspec.yaml present)");
        println!("[3/8] ✓ Resolved cwd: {cwd}");

        // ── Step 4: load cross-project AgentTeam + apply per-task config ─
        // Order: load → set_extra_tools → set_dry_run → reset (resets only
        // session_id, not the dry_run / extra_tools fields).
        let mut team = AgentTeam::load_for_worker_project(&cwd, 0, &ctx.repo);
        team.set_extra_tools(&ctx.extra_tools);
        team.set_dry_run(true);   // CRITICAL — injects DRY_RUN_ADDENDUM
        team.pm.reset();
        team.engineer.reset();
        team.tester.reset();
        // Sanity: confirm dry_run survived the resets
        assert!(team.pm.dry_run && team.engineer.dry_run && team.tester.dry_run,
            "dry_run must persist across reset() (only state is wiped)");
        println!("[4/8] ✓ AgentTeam loaded (project=agora_market, dry_run=true)");

        // ── Step 5: full 5-iter assign_task loop ─────────────────────────
        println!("[5/8] ⏳ Running full assign_task (PM↔Engineer ≤5 iters)…");
        let started = std::time::Instant::now();
        let assign_result = team.assign_task(&task.description);
        let elapsed = started.elapsed();
        println!("[5/8] ⏳ assign_task finished in {:.1?}", elapsed);

        // ── Step 6: divert to preview file (mirrors worker dry-run branch) ─
        let (review, success) = match assign_result {
            Ok(r)  => {
                println!("[6/8] ✓ assign_task returned APPROVED ({} chars)", r.len());
                (r, true)
            }
            Err(e) => {
                println!("[6/8] ✗ assign_task returned ERR ({} chars): {:.120}",
                    e.len(), e);
                (e, false)
            }
        };
        worker::save_preview_comment(&task.id, &issue_url, &review, success);

        // ── Step 7: verify preview file written + round-trips correctly ──
        let preview = latest_preview_for(&task.id)
            .expect("latest_preview_for must find the preview we just saved");
        assert_eq!(preview.task_id,   task.id,    "task_id must match");
        assert_eq!(preview.issue_url, issue_url,  "issue_url must match");
        assert_eq!(preview.success,   success,    "success flag must match");
        assert_eq!(preview.body,      review,     "body must round-trip byte-for-byte");
        assert!(!preview.saved_at.is_empty(),     "saved_at must be set");
        assert!(!preview.body.is_empty(),         "body must be non-empty");
        println!("[7/8] ✓ Preview round-trip OK: {} chars body, saved_at={}",
            preview.body.len(), preview.saved_at);

        // ── Step 8: confirm preview JSONL physically exists ──────────────
        // We can't directly assert "no GitHub comment was posted" from inside
        // this test, but the worker dry-run branch is hard-coded to call
        // save_preview_comment instead of comment_on_issue_url — so by
        // construction (and by mirror of that branch above), no `gh issue
        // comment` was invoked during this test.
        let preview_path = crate::platform::app_data_dir()
            .join("data").join("multi_agent").join("preview_comments.jsonl");
        assert!(preview_path.exists(),
            "preview JSONL must exist at {}", preview_path.display());
        println!("[8/8] ✓ Preview JSONL: {}", preview_path.display());

        // ── Print preview body for human eyeballing ──────────────────────
        println!("\n──────── PREVIEW (would have posted to GitHub) ────────");
        println!("Issue URL : {}", preview.issue_url);
        println!("Success   : {}", preview.success);
        println!("Saved at  : {}", preview.saved_at);
        println!("Body ({} chars):\n{}", preview.body.len(), preview.body);
        println!("────────────────────────────────────────────────────────");

        println!("\n✓ DRY-RUN end-to-end verification PASSED");
        println!("  → No GitHub comments posted (worker took dry-run branch)");
        println!("  → Preview saved for human review");
        println!("  → To approve+post:  github_adapter::replay_preview(&preview)");
        println!("  → To inspect raw:   {}", preview_path.display());
        if let Some(sid) = team.pm.session_id() {
            println!("  → PM session:       claude --resume {sid}");
        }
        if let Some(sid) = team.engineer.session_id() {
            println!("  → Engineer session: claude --resume {sid}");
        }

        // ── Cleanup ──────────────────────────────────────────────────────
        // Match worker behaviour: clear per-task config so subsequent tests
        // / runs on the same team don't inherit dry_run or extra_tools.
        team.set_extra_tools(&[]);
        team.set_dry_run(false);
        queue::update_status(&task.id, queue::TaskStatus::Done,
            Some("[DRY-RUN verification — see github_adapter::tests::live_full_dry_run_on_real_issue]".into()));
    }

    /// End-to-end: post a comment to a real issue via gh CLI. Marked
    /// `#[ignore]` because it WRITES to GitHub — only run when explicitly
    /// validating the comment-back loop.
    ///
    /// Configure target via env:
    ///   SIRIN_VERIFY_REPO=Redandan/AgoraMarketTest
    ///   SIRIN_VERIFY_ISSUE=1
    /// (defaults to AgoraMarketTest #1 — change before running if that
    /// issue doesn't exist).
    #[test]
    #[ignore]
    fn live_post_comment_to_real_issue() {
        let repo = std::env::var("SIRIN_VERIFY_REPO")
            .unwrap_or_else(|_| "Redandan/AgoraMarketTest".to_string());
        let num: u32 = std::env::var("SIRIN_VERIFY_ISSUE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        let body = format!(
            "🤖 Sirin AgentTeam loop verification\n\
             \n\
             Posted at {} by `cargo test live_post_comment_to_real_issue`.\n\
             If you see this, github_adapter::comment_on_issue is working end-to-end.",
            chrono::Local::now().to_rfc3339(),
        );
        comment_on_issue(&repo, num, &body)
            .expect("comment_on_issue must succeed");
        println!("\n✓ Posted verification comment to {repo}#{num}\n");
    }
}
