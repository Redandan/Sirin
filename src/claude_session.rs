//! Spawn external Claude Code CLI sessions for cross-repo bug fixing.
//!
//! Uses `claude -p` (print mode) which runs non-interactively on the user's
//! Max plan — no API key needed.

use std::path::Path;
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

    let output = Command::new("claude")
        .args(["-p", prompt, "--output-format", "text"])
        .current_dir(cwd_path)
        .output()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;

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
