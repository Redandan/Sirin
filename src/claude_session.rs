//! Spawn external Claude Code CLI sessions for cross-repo bug fixing.
//!
//! Uses `claude -p` (print mode) which runs non-interactively on the user's
//! Max plan — no API key needed.
//!
//! # Supervision pattern
//!
//! `run_supervised()` watches a primary Claude Code session and, whenever it
//! stops (question at end, max-turns hit, etc.), automatically decides what to
//! reply — either a simple "yes/continue" or by consulting a **second** Claude
//! session that can read a different repo and return an informed recommendation.
//!
//! ```text
//! Primary session  →  stops with question
//!                        ↓ Sirin detects pause
//!                  Consultant session  ←  question + context
//!                        ↓ advice
//!                  Primary session  ←  answer  →  continues
//! ```

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::platform::NoWindow;

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

// ══════════════════════════════════════════════════════════════════════════════
//  SUPERVISION — 監控主 session，遇到停頓自動決定怎麼回答
// ══════════════════════════════════════════════════════════════════════════════

/// 主 session 暫停時 Sirin 的處理策略。
#[derive(Debug, Clone)]
pub enum SupervisionPolicy {
    /// 全部直接回「yes / continue」— 適合純跑任務不需判斷的場景。
    AutoApprove,
    /// 把問題轉給另一個 Claude session（顧問），帶著答案回來繼續。
    Consult {
        /// 顧問 session 的工作目錄；None = 與主 session 相同目錄。
        consultant_cwd: Option<String>,
    },
}

/// `run_supervised` 每個步驟的事件回調 — 可轉發到 Telegram / UI。
#[derive(Debug, Clone)]
pub enum SupervisionEvent {
    /// 主 session 正在思考 / 輸出文字
    Working { text: String },
    /// 主 session 正在呼叫工具
    UsingTool { name: String },
    /// 主 session 停了，最後一句像個問題
    Paused { question: String },
    /// 去問顧問 session 中
    Consulting { question: String },
    /// 顧問回答了
    GotAdvice { advice: String },
    /// 進入下一輪（--continue）
    Continuing { round: usize },
    /// 全部完成
    Done { output: String },
}

// ── consult() ────────────────────────────────────────────────────────────────

/// 把問題轉包給另一個 Claude session，取回建議。
///
/// - `question`         — 主 session 停下來的那句話 / 問題
/// - `working_context`  — 主 session 到目前為止做了什麼（讓顧問有背景）
/// - `consultant_cwd`   — 顧問 session 的工作目錄（可以是另一個 repo）
///
/// 回傳顧問的建議文字（已 trim，簡潔可執行）。
pub fn consult(
    question: &str,
    working_context: &str,
    consultant_cwd: &str,
) -> Result<String, String> {
    let prompt = format!(
        "You are a senior technical advisor reviewing another AI coding session.\n\
         \n\
         ## What the primary session has done so far\n\
         {working_context}\n\
         \n\
         ## Question / decision point\n\
         {question}\n\
         \n\
         Give a concise, actionable recommendation (2-5 lines).\n\
         Start directly with the answer — no preamble."
    );
    let result = run_sync(consultant_cwd, &prompt)?;
    Ok(result.output.trim().to_string())
}

// ── run_supervised() ─────────────────────────────────────────────────────────

/// 執行一個「被監督的」Claude Code session。
///
/// 每當主 session 停下來（問問題 / 達到輪次上限 / 需要確認），
/// Sirin 根據 `policy` 決定怎麼回應，然後用 `--continue` 讓它繼續。
/// 最多重試 `MAX_ROUNDS` 輪，超過回傳 Err。
///
/// `on_event` 會在每個步驟被呼叫，可用來把進度推送到 Telegram 或 UI。
pub fn run_supervised(
    cwd: &str,
    initial_prompt: &str,
    policy: &SupervisionPolicy,
    on_event: &(impl Fn(SupervisionEvent) + Sync),
) -> Result<SessionResult, String> {
    let cwd_path = Path::new(cwd);
    if !cwd_path.exists() {
        return Err(format!("cwd does not exist: {cwd}"));
    }

    let mut prompt          = initial_prompt.to_string();
    let mut is_continuation = false;
    let mut context_so_far  = String::new();   // 累積，給顧問當背景
    const MAX_ROUNDS: usize = 5;

    for round in 0..MAX_ROUNDS {
        if round > 0 {
            on_event(SupervisionEvent::Continuing { round });
        }

        // ── 跑這一輪 ──────────────────────────────────────────────────────
        let (exit_code, last_text, final_output, subtype) =
            run_one_round(cwd_path, &prompt, is_continuation, on_event)?;

        context_so_far.push_str(&last_text);
        context_so_far.push('\n');

        // ── 成功退出 → 完成 ───────────────────────────────────────────────
        if exit_code == 0 || subtype == "success" {
            on_event(SupervisionEvent::Done { output: final_output.clone() });
            return Ok(SessionResult { success: true, output: final_output, exit_code });
        }

        // ── 最後一輪 → 放棄 ───────────────────────────────────────────────
        if round + 1 >= MAX_ROUNDS {
            break;
        }

        // ── 偵測問題：最後一句有 ? 或常見猶豫詞 ─────────────────────────
        let last_line = last_text.lines().last().unwrap_or("").trim().to_string();
        let question = if looks_like_question(&last_line) {
            last_line.clone()
        } else {
            "Please continue with the next step.".to_string()
        };

        on_event(SupervisionEvent::Paused { question: question.clone() });

        // ── 根據 policy 決定怎麼回答 ──────────────────────────────────────
        prompt = match policy {
            SupervisionPolicy::AutoApprove => {
                "Yes, please continue.".to_string()
            }
            SupervisionPolicy::Consult { consultant_cwd } => {
                let c_cwd = consultant_cwd.as_deref().unwrap_or(cwd);
                on_event(SupervisionEvent::Consulting { question: question.clone() });
                let advice = consult(&question, &context_so_far, c_cwd)?;
                on_event(SupervisionEvent::GotAdvice { advice: advice.clone() });
                advice
            }
        };

        is_continuation = true;
    }

    Err(format!("supervised: max rounds ({MAX_ROUNDS}) reached without success"))
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// 執行一輪 `claude -p ... --output-format stream-json`，
/// 解析每一行 JSON，透過 on_event 通知，
/// 回傳 (exit_code, last_assistant_text, final_result_text, result_subtype)。
fn run_one_round(
    cwd: &Path,
    prompt: &str,
    continuation: bool,
    on_event: &(impl Fn(SupervisionEvent) + Sync),
) -> Result<(i32, String, String, String), String> {
    let bin = claude_bin();
    let mut cmd = if bin.extension().map(|e| e == "cmd").unwrap_or(false) {
        let mut c = Command::new("cmd");
        c.no_window().arg("/c").arg(&bin);
        c
    } else {
        let mut c = Command::new(&bin);
        c.no_window();
        c
    };

    cmd.current_dir(cwd)
        .args(["-p", prompt, "--output-format", "stream-json",
               "--verbose", "--dangerously-skip-permissions"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    if continuation { cmd.arg("--continue"); }

    let mut child = cmd.spawn().map_err(|e| format!("spawn claude: {e}"))?;
    let stdout    = child.stdout.take().ok_or("no stdout")?;

    let mut last_text    = String::new();
    let mut final_output = String::new();
    let mut subtype      = String::new();

    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { continue };
        let Ok(val)  = serde_json::from_str::<serde_json::Value>(&line) else { continue };

        match val.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if let Some(text) = extract_assistant_text(&val) {
                    on_event(SupervisionEvent::Working { text: text.clone() });
                    last_text = text;
                }
            }
            Some("tool_use") => {
                let name = val["name"].as_str().unwrap_or("?").to_string();
                on_event(SupervisionEvent::UsingTool { name });
            }
            Some("result") => {
                subtype      = val["subtype"].as_str().unwrap_or("").to_string();
                final_output = val["result"].as_str().unwrap_or("").to_string();
            }
            _ => {}
        }
    }

    let exit_code = child.wait()
        .map(|s| s.code().unwrap_or(-1))
        .unwrap_or(-1);

    let out = if final_output.is_empty() { last_text.clone() } else { final_output };
    Ok((exit_code, last_text, out, subtype))
}

/// 判斷一段文字是否像個需要回答的問題。
fn looks_like_question(text: &str) -> bool {
    if text.is_empty() { return false; }
    text.ends_with('?')
        || text.to_lowercase().contains("should i")
        || text.to_lowercase().contains("do you want")
        || text.to_lowercase().contains("which approach")
        || text.to_lowercase().contains("would you like")
        || text.to_lowercase().contains("shall i")
}

/// 從 stream-json assistant message 中抽出純文字。
/// Single-turn run that captures the `session_id` emitted in the stream-json
/// result line.  Used by `multi_agent::PersistentSession` to track conversation
/// continuity across calls.
///
/// Returns `(assistant_output, session_id)`.
///
/// This is a thin wrapper around [`run_one_turn_scoped`] with `allowed_tools = None`
/// (i.e. `--dangerously-skip-permissions`).  Existing call-sites require no change.
pub fn run_one_turn(
    cwd: &str,
    prompt: &str,
    continuation: bool,
) -> Result<(String, String), String> {
    run_one_turn_scoped(cwd, prompt, continuation, None)
}

/// Same as [`run_one_turn`] but with an optional per-role tool whitelist.
///
/// - `allowed_tools = None`       → uses `--dangerously-skip-permissions` (god mode, existing behaviour).
/// - `allowed_tools = Some(&[…])` → uses `--allowedTools "<comma-joined>"` instead; no skip flag.
///
/// Used by `multi_agent::PersistentSession` to restrict each role to only the
/// tools it actually needs, reducing blast radius and preventing cross-role writes.
pub fn run_one_turn_scoped(
    cwd: &str,
    prompt: &str,
    continuation: bool,
    allowed_tools: Option<&[&str]>,
) -> Result<(String, String), String> {
    let cwd_path = Path::new(cwd);
    if !cwd_path.exists() {
        return Err(format!("cwd does not exist: {cwd}"));
    }

    // Build base args; permission flag depends on whitelist.
    let joined; // must outlive `args`
    let mut args = vec!["-p", prompt, "--output-format", "stream-json", "--verbose"];
    match allowed_tools {
        None => {
            args.push("--dangerously-skip-permissions");
        }
        Some(tools) => {
            joined = tools.join(",");
            args.push("--allowedTools");
            args.push(&joined);
        }
    }
    if continuation { args.push("--continue"); }

    // Fix B: stream stdout line-by-line instead of buffering all 80-100 MB of
    // stream-json into a Vec<u8>.  Bypasses run_claude()'s read_to_end()
    // because we don't need the full output — we only keep the final
    // assistant text + session_id.  Each parsed line is dropped immediately,
    // capping per-call peak memory at ~max(line_size) + len(output).
    //
    // stderr is dropped at OS level (Stdio::null) — caller never reads it
    // anyway, and capturing it would re-introduce buffering pressure.
    //
    // Hard wall-clock timeout enforced by a watchdog thread that kills the
    // child after 600s.  If the child is killed, BufReader hits EOF naturally
    // and we proceed to wait().
    let bin = claude_bin();
    let mut cmd = if bin.extension().map(|e| e == "cmd").unwrap_or(false) {
        let mut c = Command::new("cmd");
        c.no_window().arg("/c").arg(&bin);
        c
    } else {
        let mut c = Command::new(&bin);
        c.no_window();
        c
    };
    cmd.current_dir(cwd_path)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd.spawn().map_err(|e| format!("spawn claude: {e}"))?;
    let stdout    = child.stdout.take().ok_or("no stdout")?;

    // Watchdog: kill the child after 600s if still running.
    let pid = child.id();
    let watchdog_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_done_clone = watchdog_done.clone();
    let watchdog = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(600);
        while Instant::now() < deadline {
            if watchdog_done_clone.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        // Timeout — try to kill via taskkill (cross-platform fallback).
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .no_window()
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .output();
        }
        #[cfg(not(windows))]
        {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).output();
        }
    });

    let mut output     = String::new();
    let mut session_id = String::new();

    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { continue };
        let Ok(val)  = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        match val["type"].as_str() {
            Some("assistant") => {
                if let Some(t) = extract_assistant_text(&val) { output = t; }
            }
            Some("result") => {
                if let Some(r) = val["result"].as_str()     { output     = r.to_string(); }
                if let Some(s) = val["session_id"].as_str() { session_id = s.to_string(); }
            }
            _ => {}
        }
        // `line` and `val` drop here — per-iteration memory freed.
    }

    // Signal watchdog we're done so it stops waiting.
    watchdog_done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = child.wait();
    let _ = watchdog.join();

    Ok((output, session_id))
}

fn extract_assistant_text(val: &serde_json::Value) -> Option<String> {
    let blocks = val.get("message")?.get("content")?.as_array()?;
    let texts: Vec<&str> = blocks.iter().filter_map(|b| {
        if b.get("type")?.as_str()? == "text" { b["text"].as_str() } else { None }
    }).collect();
    if texts.is_empty() { None } else { Some(texts.join("")) }
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
        if Command::new(c).no_window().arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
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
///
/// Thin wrapper around `run_claude_with_timeout` with a 10-minute default.
fn run_claude(args: &[&str], cwd: Option<&Path>) -> Result<std::process::Output, String> {
    run_claude_with_timeout(args, cwd, Duration::from_secs(600))
}

/// Run claude with a hard wall-clock timeout.
///
/// If the subprocess does not complete within `timeout`, the child is killed
/// and `Err("claude subprocess timed out after Ns")` is returned.  This
/// prevents squad worker threads from blocking indefinitely when the
/// Anthropic API hangs or rate-limits at high concurrency (N=4 workers).
///
/// On Windows, `claude` is installed as `claude.cmd`. We detect this and
/// invoke Node.js directly (not `cmd /c`) because cmd.exe treats embedded
/// newlines as command separators, truncating multi-line prompts.
fn run_claude_with_timeout(
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let bin = claude_bin();

    let mut cmd = if bin.extension().map(|e| e == "cmd").unwrap_or(false) {
        // Resolve the Node.js entry point that claude.cmd wraps.
        // claude.cmd lives at e.g. %APPDATA%\npm\claude.cmd
        // The actual JS is at %APPDATA%\npm\node_modules\@anthropic-ai\claude-code\cli.js
        //
        // We invoke Node directly (not `cmd /c claude.cmd`) because cmd.exe treats
        // embedded newlines in arguments as command separators, which truncates
        // multi-line prompts and strips flags like --output-format stream-json.
        let node_script = resolve_claude_node_script(&bin);
        match node_script {
            Some(script) => {
                let mut c = Command::new("node");
                c.no_window().arg(script);
                c
            }
            None => {
                // Fallback: cmd /c (may break for multi-line prompts on Windows)
                let mut c = Command::new("cmd");
                c.no_window().arg("/c").arg(&bin);
                c
            }
        }
    } else {
        let mut c = Command::new(&bin);
        c.no_window();
        c
    };

    cmd.args(args);
    if let Some(dir) = cwd { cmd.current_dir(dir); }
    // Pipe stdout/stderr so we can drain them in background threads.
    // Explicitly null stdin: Claude CLI waits 3s for stdin if inherited.
    cmd.stdin(Stdio::null())
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());

    let child = cmd.spawn().map_err(|e| format!("spawn claude ({bin:?}): {e}"))?;
    wait_child_with_timeout(child, timeout)
}

/// Drive a spawned child to completion, killing it if it exceeds `timeout`.
///
/// Stdout and stderr are drained on background threads to prevent the OS
/// pipe buffer from filling up and blocking the child process.
fn wait_child_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut stdout_pipe = child.stdout.take().ok_or("no stdout pipe".to_string())?;
    let mut stderr_pipe = child.stderr.take().ok_or("no stderr pipe".to_string())?;

    // Drain pipes on background threads; the child may not exit until its
    // stdout buffer is consumed (OS pipe buffer is typically 64 KB).
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => {
                let stdout = stdout_handle.join().unwrap_or_default();
                let stderr = stderr_handle.join().unwrap_or_default();
                return Ok(std::process::Output { status, stdout, stderr });
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "claude subprocess timed out after {}s",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Find the Node.js script that backs `claude.cmd`.
/// claude.cmd is an npm shim; the real entry point is:
///   <npm_prefix>\node_modules\@anthropic-ai\claude-code\cli.js
fn resolve_claude_node_script(claude_cmd: &Path) -> Option<PathBuf> {
    // When claude_bin() returns a bare filename like "claude.cmd" (no directory),
    // parent() returns an empty path.  Fall back to the standard npm global location.
    let npm_dir: PathBuf = {
        let parent = claude_cmd.parent().unwrap_or(Path::new(""));
        if parent.as_os_str().is_empty() {
            // Bare filename — locate via %APPDATA%\npm (Windows npm global)
            let appdata = std::env::var("APPDATA").ok()?;
            PathBuf::from(appdata).join("npm")
        } else {
            parent.to_path_buf()
        }
    };
    let candidate = npm_dir
        .join("node_modules")
        .join("@anthropic-ai")
        .join("claude-code")
        .join("cli.js");
    if candidate.exists() { Some(candidate) } else { None }
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
    fn test_subprocess_timeout_kills_child() {
        // Spawn a long-running process (30 s), apply a 2-second timeout.
        // Expects Err containing "timed out"; child must not become a zombie.
        #[cfg(windows)]
        let mut cmd = {
            // ping /n 31 loops for ~30 s without requiring interactive stdin
            let mut c = Command::new("ping");
            c.args(["/n", "31", "127.0.0.1"]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        cmd.stdin(Stdio::null())
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());

        let child = cmd.spawn().expect("spawn long-running process");
        let result = wait_child_with_timeout(child, Duration::from_secs(2));
        assert!(result.is_err(), "expected timeout error, got {:?}", result);
        assert!(
            result.unwrap_err().contains("timed out"),
            "error message should mention 'timed out'"
        );
    }

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

    /// Verify that `run_one_turn_scoped` selects the right permission flag.
    ///
    /// We cannot actually spawn Claude in unit tests, but we can inspect the
    /// args that *would* be built.  The helper below mirrors the arg-building
    /// logic so we can assert without I/O.
    #[test]
    fn scoped_args_none_uses_skip_permissions() {
        // None → --dangerously-skip-permissions present, --allowedTools absent
        let args = build_scoped_args("hello", false, None);
        assert!(args.contains(&"--dangerously-skip-permissions"),
            "None should add --dangerously-skip-permissions");
        assert!(!args.iter().any(|a| *a == "--allowedTools"),
            "None must NOT add --allowedTools");
    }

    #[test]
    fn scoped_args_some_uses_allowed_tools() {
        let tools = &["Read", "Grep", "Glob"];
        let args = build_scoped_args("hello", false, Some(tools));
        assert!(!args.contains(&"--dangerously-skip-permissions"),
            "Some(tools) must NOT add --dangerously-skip-permissions");
        let idx = args.iter().position(|a| *a == "--allowedTools")
            .expect("--allowedTools must be present");
        assert_eq!(args[idx + 1], "Read,Grep,Glob",
            "tools must be comma-joined");
    }

    #[test]
    fn scoped_args_continuation_appended() {
        let args_no_cont  = build_scoped_args("p", false, None);
        let args_cont     = build_scoped_args("p", true,  None);
        assert!(!args_no_cont.contains(&"--continue"));
        assert!( args_cont.contains(&"--continue"));
    }

    /// Mirror of the arg-building logic in `run_one_turn_scoped`, without I/O.
    fn build_scoped_args<'a>(
        prompt: &'a str,
        continuation: bool,
        allowed_tools: Option<&'a [&'a str]>,
    ) -> Vec<&'a str> {
        // We need a place to store the joined string; in the real function it's
        // a local `joined` variable.  Here we leak a tiny allocation so the
        // &str lives long enough for the test assertion.
        let mut args: Vec<&str> = vec!["-p", prompt, "--output-format", "stream-json", "--verbose"];
        match allowed_tools {
            None => { args.push("--dangerously-skip-permissions"); }
            Some(tools) => {
                let joined: &'static str = Box::leak(tools.join(",").into_boxed_str());
                args.push("--allowedTools");
                args.push(joined);
            }
        }
        if continuation { args.push("--continue"); }
        args
    }

    #[test]
    fn looks_like_question_detects_patterns() {
        assert!(looks_like_question("Should I refactor this?"));
        assert!(looks_like_question("Do you want me to proceed?"));
        assert!(looks_like_question("Which approach is better?"));
        assert!(looks_like_question("Shall I push the commit?"));
        assert!(looks_like_question("Is this correct?"));
        assert!(!looks_like_question("I have fixed the bug."));
        assert!(!looks_like_question(""));
        assert!(!looks_like_question("Running tests now..."));
    }

    #[test]
    fn extract_assistant_text_parses_content() {
        let val: serde_json::Value = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "Hello "},
                    {"type": "tool_use", "name": "Read"},
                    {"type": "text", "text": "world"}
                ]
            }
        });
        let text = extract_assistant_text(&val).unwrap();
        assert_eq!(text, "Hello world");
    }

    #[test]
    #[ignore] // needs Claude CLI + Max plan
    fn consult_returns_advice() {
        let cwd = repo_path("sirin").expect("sirin path");
        let advice = consult(
            "Should I use a HashMap or BTreeMap for storing agent IDs?",
            "Working on src/agents/mod.rs, adding an agent registry.",
            &cwd,
        ).expect("consult");
        println!("Advice: {advice}");
        assert!(!advice.is_empty());
    }

    #[test]
    #[ignore] // needs Claude CLI + Max plan
    fn supervised_auto_approve() {
        let cwd = repo_path("sirin").expect("sirin path");
        let events = std::sync::Mutex::new(Vec::new());
        let result = run_supervised(
            &cwd,
            "Reply with exactly: SUPERVISED_OK",
            &SupervisionPolicy::AutoApprove,
            &|e| { events.lock().unwrap().push(format!("{e:?}")); },
        ).expect("supervised");
        println!("Output: {}", result.output);
        println!("Events: {:?}", events.lock().unwrap());
        assert!(result.output.contains("SUPERVISED_OK"));
    }

    #[test]
    #[ignore] // needs Claude CLI + Max plan
    fn supervised_consult_pattern() {
        let cwd = repo_path("sirin").expect("sirin path");
        let result = run_supervised(
            &cwd,
            "Look at src/claude_session.rs, then ask me whether you should \
             add more tests. Wait for my answer before proceeding.",
            &SupervisionPolicy::Consult { consultant_cwd: Some(cwd.clone()) },
            &|e| println!("[event] {e:?}"),
        ).expect("supervised consult");
        println!("Final: {}", result.output);
        assert!(result.success);
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
