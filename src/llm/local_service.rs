//! Local LLM service bootstrap helpers.
//!
//! LM Studio is a desktop app plus a local OpenAI-compatible server.  Users
//! often have the app installed but the server is not currently running.  Sirin
//! treats that as recoverable for localhost LM Studio configs: start the local
//! server, wait briefly, then retry the original probe/call.

use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn autostart_enabled() -> bool {
    !matches!(
        std::env::var("SIRIN_LMSTUDIO_AUTOSTART")
            .unwrap_or_else(|_| "1".into())
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn autostart_wait() -> Duration {
    let secs = std::env::var("SIRIN_LMSTUDIO_AUTOSTART_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(20);
    Duration::from_secs(secs)
}

pub(crate) fn is_local_lmstudio_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let host = url
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
}

fn port_from_base_url(base_url: &str) -> Option<u16> {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.port_or_known_default())
}

async fn lmstudio_models_endpoint_ready(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> bool {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(url).timeout(Duration::from_secs(2));
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    matches!(req.send().await, Ok(resp) if resp.status().is_success())
}

fn spawn_hidden(mut cmd: Command) -> Result<(), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn failed: {e}"))
}

fn spawn_lms_server(base_url: &str) -> Result<(), String> {
    let exe = std::env::var("LMS_CLI")
        .or_else(|_| std::env::var("LM_STUDIO_CLI"))
        .unwrap_or_else(|_| "lms".into());
    let mut cmd = Command::new(exe);
    cmd.args(["server", "start", "--bind", "127.0.0.1"]);
    if let Some(port) = port_from_base_url(base_url) {
        cmd.arg("--port").arg(port.to_string());
    }
    spawn_hidden(cmd)
}

#[cfg(windows)]
fn lmstudio_app_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("LM_STUDIO_APP_PATH") {
        if !path.trim().is_empty() {
            candidates.push(std::path::PathBuf::from(path));
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(
            std::path::PathBuf::from(local)
                .join("Programs")
                .join("LM Studio")
                .join("LM Studio.exe"),
        );
    }
    if let Ok(program_files) = std::env::var("PROGRAMFILES") {
        candidates.push(
            std::path::PathBuf::from(program_files)
                .join("LM Studio")
                .join("LM Studio.exe"),
        );
    }
    if let Ok(program_files_x86) = std::env::var("PROGRAMFILES(X86)") {
        candidates.push(
            std::path::PathBuf::from(program_files_x86)
                .join("LM Studio")
                .join("LM Studio.exe"),
        );
    }
    candidates
}

#[cfg(not(windows))]
fn lmstudio_app_candidates() -> Vec<std::path::PathBuf> {
    std::env::var("LM_STUDIO_APP_PATH")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(std::path::PathBuf::from)
        .into_iter()
        .collect()
}

fn spawn_lmstudio_app() -> Result<(), String> {
    let path = lmstudio_app_candidates()
        .into_iter()
        .find(|path| path.exists())
        .ok_or_else(|| "LM Studio app executable not found".to_string())?;
    spawn_hidden(Command::new(path))
}

fn recent_autostart_cell() -> &'static Mutex<Option<Instant>> {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

fn should_attempt_autostart_now() -> bool {
    let mut guard = recent_autostart_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if guard.is_some_and(|last| last.elapsed() < Duration::from_secs(60)) {
        return false;
    }
    *guard = Some(Instant::now());
    true
}

/// Ensure a localhost LM Studio server is reachable.
///
/// This is intentionally best-effort.  If the endpoint is already up, it is a
/// cheap `/models` check.  If it is down, Sirin tries `lms server start` first
/// and falls back to launching the GUI executable.  The caller can then retry
/// its original model-list or completion request.
pub(crate) async fn ensure_lmstudio_server(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<(), String> {
    if !autostart_enabled() || !is_local_lmstudio_url(base_url) {
        return Ok(());
    }
    if lmstudio_models_endpoint_ready(client, base_url, api_key).await {
        return Ok(());
    }
    if !should_attempt_autostart_now() {
        return Ok(());
    }

    crate::sirin_log!(
        "[llm] LM Studio at {} is not reachable; attempting auto-start",
        base_url
    );

    let cli_result = spawn_lms_server(base_url);
    if let Err(e) = &cli_result {
        crate::sirin_log!("[llm] lms server start unavailable: {e}; trying LM Studio app");
        spawn_lmstudio_app()?;
    }

    let deadline = Instant::now() + autostart_wait();
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if lmstudio_models_endpoint_ready(client, base_url, api_key).await {
            crate::sirin_log!("[llm] LM Studio server is ready at {}", base_url);
            return Ok(());
        }
    }

    Err(format!(
        "LM Studio auto-start attempted but {base_url}/models was not reachable within {}s",
        autostart_wait().as_secs()
    ))
}

#[cfg(test)]
mod tests {
    use super::{is_local_lmstudio_url, port_from_base_url};

    #[test]
    fn detects_local_lmstudio_urls() {
        assert!(is_local_lmstudio_url("http://localhost:1234/v1"));
        assert!(is_local_lmstudio_url("http://127.0.0.1:1234/v1"));
        assert!(is_local_lmstudio_url("http://[::1]:1234/v1"));
        assert!(!is_local_lmstudio_url("https://api.openai.com/v1"));
        assert!(!is_local_lmstudio_url("not a url"));
    }

    #[test]
    fn parses_port_from_base_url() {
        assert_eq!(port_from_base_url("http://localhost:1234/v1"), Some(1234));
        assert_eq!(port_from_base_url("https://localhost/v1"), Some(443));
    }
}
