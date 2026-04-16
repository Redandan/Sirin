//! Spawn external Claude Code CLI sessions for cross-repo bug fixing.
//!
//! Uses `claude -p` (print mode) which runs non-interactively on the user's
//! Max plan — no API key needed.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Result of a spawned Claude session.
#[derive(Debug, Clone)]
pub struct SessionResult {
    pub success: bool,
    pub output: String,
    pub exit_code: i32,
}

/// Spawn a Claude Code CLI session synchronously.
/// `cwd` is the working directory (repo path).
/// `prompt` is the full instruction to Claude.
pub fn run_sync(cwd: &str, prompt: &str) -> Result<SessionResult, String> {
    let cwd_path = Path::new(cwd);
    if !cwd_path.exists() {
        return Err(format!("cwd does not exist: {cwd}"));
    }

    let output = run_claude(&["-p", prompt, "--output-format", "text"], Some(cwd_path))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    let combined = if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}\n--- stderr ---\n{stderr}")
    };

    Ok(SessionResult {
        success: output.status.success(),
        output: combined,
        exit_code,
    })
}

/// Spawn a Claude session in background (returns a join handle).
pub fn run_async(
    cwd: String,
    prompt: String,
) -> std::thread::JoinHandle<Result<SessionResult, String>> {
    std::thread::spawn(move || run_sync(&cwd, &prompt))
}

/// Build a bug-fix prompt from test context.
pub fn build_bug_prompt(
    bug_description: &str,
    url: Option<&str>,
    error_message: Option<&str>,
    network_log: Option<&str>,
    screenshot_path: Option<&str>,
) -> String {
    let mut parts = vec![
        format!("## Browser Test Failure\n\n{bug_description}"),
    ];
    if let Some(u) = url {
        parts.push(format!("**URL:** {u}"));
    }
    if let Some(e) = error_message {
        parts.push(format!("**Error:** ```\n{e}\n```"));
    }
    if let Some(n) = network_log {
        parts.push(format!("**Network log:** ```\n{n}\n```"));
    }
    if let Some(s) = screenshot_path {
        parts.push(format!("**Screenshot saved at:** {s}"));
    }
    parts.push("\nPlease investigate and fix the issue. Run tests after fixing.".into());
    parts.join("\n\n")
}

/// Well-known repo paths (configurable via env).
pub fn repo_path(name: &str) -> Option<String> {
    // Check env first: SIRIN_REPO_BACKEND, SIRIN_REPO_FRONTEND, etc.
    let env_key = format!("SIRIN_REPO_{}", name.to_uppercase());
    if let Ok(p) = std::env::var(&env_key) {
        return Some(p);
    }
    // Defaults based on known project structure
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    match name.to_lowercase().as_str() {
        "backend" | "api" => Some(format!("{home}/IdeaProjects/AgoraMarketAPI")),
        "frontend" | "flutter" | "pwa" => Some(format!("{home}/IdeaProjects/AgoraMarketFlutter")),
        "sirin" => Some(format!("{home}/IdeaProjects/Sirin")),
        _ => None,
    }
}

/// Resolve the `claude` binary path — checks PATH, npm global, and common locations.
fn claude_bin() -> PathBuf {
    // Check env override
    if let Ok(p) = std::env::var("SIRIN_CLAUDE_BIN") {
        return PathBuf::from(p);
    }
    // Try well-known locations (Windows npm global)
    let candidates = [
        "claude",
        "claude.cmd",
    ];
    for c in candidates {
        if Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return PathBuf::from(c);
        }
    }
    // npm global path on Windows
    if let Ok(home) = std::env::var("APPDATA") {
        let npm_bin = PathBuf::from(&home).join("npm").join("claude.cmd");
        if npm_bin.exists() { return npm_bin; }
    }
    PathBuf::from("claude")
}

/// Run the claude binary with given args, handling .cmd on Windows.
fn run_claude(args: &[&str], cwd: Option<&Path>) -> Result<std::process::Output, String> {
    let bin = claude_bin();
    let mut cmd = if bin.extension().map(|e| e == "cmd").unwrap_or(false) {
        let mut c = Command::new("cmd");
        c.arg("/c").arg(&bin);
        c
    } else {
        Command::new(&bin)
    };
    cmd.args(args);
    if let Some(dir) = cwd { cmd.current_dir(dir); }
    cmd.output().map_err(|e| format!("claude failed ({bin:?}): {e}"))
}

/// Check if Claude CLI is available on the system.
pub fn cli_available() -> bool {
    run_claude(&["--version"], None).map(|o| o.status.success()).unwrap_or(false)
}

/// Get Claude CLI version string.
pub fn cli_version() -> Result<String, String> {
    let output = run_claude(&["--version"], None)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_resolves_aliases() {
        let backend = repo_path("backend");
        assert!(backend.is_some(), "backend should resolve");
        assert!(backend.unwrap().contains("AgoraMarketAPI"));

        let frontend = repo_path("frontend");
        assert!(frontend.is_some());

        let sirin = repo_path("sirin");
        assert!(sirin.is_some());
        assert!(sirin.unwrap().contains("Sirin"));

        assert!(repo_path("nonexistent").is_none());
    }

    #[test]
    fn build_bug_prompt_formats_correctly() {
        let prompt = build_bug_prompt(
            "Login button returns 500",
            Some("https://example.com/login"),
            Some("NullPointerException"),
            Some("{\"status\":500}"),
            Some("data/screenshot.png"),
        );
        assert!(prompt.contains("Login button returns 500"));
        assert!(prompt.contains("https://example.com/login"));
        assert!(prompt.contains("NullPointerException"));
        assert!(prompt.contains("screenshot.png"));
        assert!(prompt.contains("Please investigate"));
    }

    #[test]
    fn build_bug_prompt_minimal() {
        let prompt = build_bug_prompt("something broke", None, None, None, None);
        assert!(prompt.contains("something broke"));
        assert!(prompt.contains("Please investigate"));
    }

    #[test]
    #[ignore] // needs Claude CLI installed
    fn claude_cli_available() {
        assert!(cli_available(), "Claude CLI should be installed");
        let version = cli_version().expect("version");
        println!("Claude CLI version: {version}");
        assert!(!version.is_empty());
    }

    #[test]
    #[ignore] // needs Claude CLI + Max plan
    fn claude_session_quick_test() {
        let cwd = repo_path("sirin").expect("sirin path");
        let result = run_sync(&cwd, "Reply with exactly: SIRIN_TEST_OK").expect("run_sync");
        println!("Exit code: {}", result.exit_code);
        println!("Output: {}", &result.output[..result.output.len().min(500)]);
        assert!(result.success, "session should succeed");
        assert!(result.output.contains("SIRIN_TEST_OK"), "output should contain marker");
    }

    /// Full pipeline: browser test → detect issue → build prompt → spawn Claude
    /// Run: cargo test --bin sirin full_pipeline -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore] // needs Chrome + Claude CLI + network
    fn full_pipeline() {
        use crate::browser;

        println!("=== Phase 1: Browser Test ===");
        browser::close();
        browser::ensure_open(true).expect("launch browser");

        // Navigate to the wiki (known working page)
        browser::navigate("https://github.com/Redandan/Redandan.github.io/wiki").expect("nav");
        std::thread::sleep(std::time::Duration::from_secs(3));

        let title = browser::page_title().unwrap_or_default();
        println!("[1] Page title: {title}");

        // Check for expected content
        let has_agora = browser::evaluate_js(
            "document.body.innerText.includes('Agora Market')"
        ).unwrap_or_default();
        println!("[2] Contains 'Agora Market': {has_agora}");

        // Simulate finding a "bug" — check for a non-existent element
        let missing = !browser::element_exists("#nonexistent-feature-xyz").unwrap_or(true);
        println!("[3] Missing element detected: {missing}");

        // Capture network + console context
        browser::install_console_capture().ok();
        browser::install_network_capture().ok();
        let console = browser::console_messages(5).unwrap_or_default();
        println!("[4] Console: {console}");

        // Take screenshot as evidence
        let png = browser::screenshot().expect("screenshot");
        std::fs::create_dir_all("data").ok();
        let shot_path = "data/test_pipeline_screenshot.png";
        std::fs::write(shot_path, &png).expect("save");
        println!("[5] Screenshot: {shot_path} ({} bytes)", png.len());

        let url = browser::current_url().unwrap_or_default();
        browser::close();

        println!("\n=== Phase 2: Build Bug Report ===");
        let bug_prompt = build_bug_prompt(
            "Wiki page is missing #nonexistent-feature-xyz element. \
             This is a simulated test bug — reply with: PIPELINE_TEST_OK",
            Some(&url),
            Some("Element not found: #nonexistent-feature-xyz"),
            None,
            Some(shot_path),
        );
        println!("[6] Prompt built ({} chars)", bug_prompt.len());
        println!("{}", &bug_prompt[..bug_prompt.len().min(300)]);

        println!("\n=== Phase 3: Spawn Claude Session ===");
        let cwd = repo_path("sirin").expect("sirin path");
        let result = run_sync(&cwd, &bug_prompt).expect("claude session");
        println!("[7] Exit code: {}", result.exit_code);
        println!("[8] Output:\n{}", &result.output[..result.output.len().min(500)]);

        assert!(result.success, "Claude session should succeed");
        assert!(!result.output.is_empty(), "Claude should produce output");

        println!("\n✓ full_pipeline: Browser → Bug Report → Claude Session — all passed");
    }
}
