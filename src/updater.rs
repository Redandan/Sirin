//! Auto-update: checks GitHub Releases for a newer version, downloads
//! and replaces the binary, then signals the UI to prompt a restart.
//!
//! ## Flow
//! 1. `spawn_check()` — fires once on startup, background thread.
//!    Puts result into `UPDATE_STATE`.
//! 2. UI polls `get_status()` each frame — shows banner if Some.
//! 3. User clicks "Update" → `apply_update()`.
//!    Windows: downloads SirinSetup-{ver}.exe, spawns it (UAC), self-exits.
//!    Other:   opens GitHub Releases in browser (no self-update support yet).
//!
//! ## No `self_update` crate dependency
//! Version check uses the GitHub Releases API directly via `reqwest`.
//! This avoids pulling in a second reqwest copy (0.12) + zip + indicatif
//! that the self_update crate brought as transitive deps.

use std::sync::{Mutex, OnceLock};

// ── Public types ─────────────────────────────────────────────────────────────

/// Information about an available update.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub release_notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// No check done yet / disabled
    Idle,
    /// Check in progress
    Checking,
    /// Newer version found — show banner
    Available(String),
    /// Already up to date
    UpToDate,
    /// Check failed (non-fatal)
    CheckFailed(String),
    /// Downloading + replacing binary
    Applying,
    /// Done — user must restart
    RestartRequired,
    /// Apply failed
    ApplyFailed(String),
}

// ── Global state ─────────────────────────────────────────────────────────────

static UPDATE_STATE: OnceLock<Mutex<UpdateStatus>> = OnceLock::new();

fn state() -> &'static Mutex<UpdateStatus> {
    UPDATE_STATE.get_or_init(|| Mutex::new(UpdateStatus::Idle))
}

pub fn get_status() -> UpdateStatus {
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn set_status(s: UpdateStatus) {
    *state().lock().unwrap_or_else(|e| e.into_inner()) = s;
}

// ── GitHub coordinates ────────────────────────────────────────────────────────

const OWNER: &str = "Redandan";
const REPO:  &str = "Sirin";

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the current version string from Cargo.toml.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// URL to the GitHub Releases "latest" page — opened in browser as fallback.
pub fn release_page_url() -> String {
    format!("https://github.com/{OWNER}/{REPO}/releases/latest")
}

/// Best-effort check whether we can actually replace the running binary.
/// Used on non-Windows only (Windows uses the installer flow).
#[cfg(any(test, not(target_os = "windows")))]
fn check_binary_writable() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe.parent()
        .ok_or_else(|| format!("binary has no parent dir: {exe:?}"))?;
    let probe = dir.join(".sirin_update_probe");
    match std::fs::write(&probe, b"x") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(format!(
            "Sirin 安裝在 {} — 自動更新需要該資料夾的寫入權限。失敗原因：{}",
            dir.display(), e
        )),
    }
}

/// Spawn a background thread that checks for updates once.
/// Non-blocking — call at startup, poll `get_status()` in the UI loop.
pub fn spawn_check() {
    set_status(UpdateStatus::Checking);
    std::thread::spawn(|| {
        match check_once() {
            Ok(Some(info)) => set_status(UpdateStatus::Available(info.version)),
            Ok(None)        => set_status(UpdateStatus::UpToDate),
            Err(e)          => set_status(UpdateStatus::CheckFailed(e)),
        }
    });
}

/// Blocking — call from a background thread.
///
/// ## Windows (v0.4.1+)
/// Downloads `SirinSetup-{version}.exe`, spawns with `/SILENT /SUPPRESSMSGBOXES`.
/// Inno Setup auto-triggers UAC. Sirin self-exits 2s later.
///
/// ## Non-Windows
/// Opens GitHub Releases in the browser — no self-replace support yet.
pub fn apply_update(version: &str) -> Result<(), String> {
    set_status(UpdateStatus::Applying);

    // ── Windows: download installer + UAC elevation ──────────────────────────
    #[cfg(target_os = "windows")]
    {
        match download_and_run_installer(version) {
            Ok(()) => {
                set_status(UpdateStatus::RestartRequired);
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    std::process::exit(0);
                });
                Ok(())
            }
            Err(e) => {
                let msg = friendlier_io_error(&format!("installer flow: {e}"));
                set_status(UpdateStatus::ApplyFailed(msg.clone()));
                Err(msg)
            }
        }
    }

    // ── Non-Windows: open browser to releases page ───────────────────────────
    // Full self-replace for macOS/Linux is not yet implemented.
    // The UI shows a 📥 button pointing here anyway, so this path is mostly
    // a no-op placeholder.
    #[cfg(not(target_os = "windows"))]
    {
        let _ = version; // suppress unused warning
        let msg = format!(
            "此平台尚未支援自動更新。請從 GitHub Releases 手動下載：{}",
            release_page_url()
        );
        set_status(UpdateStatus::ApplyFailed(msg.clone()));
        Err(msg)
    }
}

/// Download `SirinSetup-{version}.exe` from GitHub Releases and spawn it.
#[cfg(target_os = "windows")]
fn download_and_run_installer(version: &str) -> Result<(), String> {
    let url = format!(
        "https://github.com/{OWNER}/{REPO}/releases/download/v{version}/SirinSetup-{version}.exe"
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!("sirin-updater/{}", current_version()))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let resp = client.get(&url)
        .send()
        .map_err(|e| format!("download installer: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "installer asset not found (HTTP {}): {url}",
            resp.status()
        ));
    }

    let bytes = resp.bytes()
        .map_err(|e| format!("read installer body: {e}"))?;

    let installer_path = std::env::temp_dir()
        .join(format!("SirinSetup-{version}.exe"));
    std::fs::write(&installer_path, &bytes)
        .map_err(|e| format!("write installer to {}: {e}", installer_path.display()))?;

    use crate::platform::NoWindow;
    std::process::Command::new(&installer_path)
        .no_window()
        .args(["/SILENT", "/SUPPRESSMSGBOXES"])
        .spawn()
        .map_err(|e| format!("spawn installer: {e}"))?;

    Ok(())
}

/// Translate raw os error 5 / 存取被拒 into an actionable zh-TW message.
fn friendlier_io_error(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("access is denied")
        || lower.contains("os error 5")
        || raw.contains("存取被拒")
    {
        format!(
            "更新需要寫入 Sirin 安裝資料夾，但被 Windows 拒絕（admin-only path）。\
             \n建議：從 GitHub Releases 手動下載新 installer。\
             \n原始錯誤：{raw}"
        )
    } else {
        format!("update: {raw}")
    }
}

// ── Internal: version check via GitHub Releases API ──────────────────────────

/// Hit `GET /repos/{owner}/{repo}/releases/latest` and compare with current.
/// Returns `Ok(Some(info))` when a newer release exists, `Ok(None)` when
/// already up to date.  No `self_update` dependency — pure reqwest + serde_json.
fn check_once() -> Result<Option<UpdateInfo>, String> {
    let url = format!(
        "https://api.github.com/repos/{OWNER}/{REPO}/releases/latest"
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("sirin-updater/{}", current_version()))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let resp = client.get(&url)
        .send()
        .map_err(|e| format!("fetch releases: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("GitHub API returned HTTP {}", resp.status()));
    }

    let json: serde_json::Value = resp.json()
        .map_err(|e| format!("parse release JSON: {e}"))?;

    let tag = json["tag_name"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches('v');

    if tag.is_empty() {
        return Ok(None);
    }

    let release_notes = json["body"].as_str().unwrap_or("").to_string();

    if semver_gt(tag, current_version()) {
        Ok(Some(UpdateInfo {
            version: tag.to_string(),
            release_notes,
        }))
    } else {
        Ok(None)
    }
}

/// Returns true when `candidate` > `current` (simple MAJOR.MINOR.PATCH).
fn semver_gt(candidate: &str, current: &str) -> bool {
    fn parse(s: &str) -> (u32, u32, u32) {
        let mut it = s.splitn(4, '.');
        let major = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let minor = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let patch = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    }
    parse(candidate) > parse(current)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_page_url_points_at_latest() {
        let url = release_page_url();
        assert!(url.starts_with("https://github.com/"));
        assert!(url.ends_with("/releases/latest"));
    }

    #[test]
    fn semver_gt_detects_newer_patch() {
        assert!(semver_gt("0.4.2", "0.4.1"));
        assert!(semver_gt("0.5.0", "0.4.9"));
        assert!(semver_gt("1.0.0", "0.9.9"));
    }

    #[test]
    fn semver_gt_rejects_older_or_equal() {
        assert!(!semver_gt("0.4.1", "0.4.1")); // equal
        assert!(!semver_gt("0.4.0", "0.4.1")); // older patch
        assert!(!semver_gt("0.3.9", "0.4.0")); // older minor
    }

    #[test]
    fn friendlier_io_error_catches_access_denied_english() {
        let raw = "IoError: Access is denied. (os error 5)";
        let friendly = friendlier_io_error(raw);
        assert!(friendly.contains("admin-only"), "got: {friendly}");
        assert!(friendly.contains("手動下載"), "got: {friendly}");
    }

    #[test]
    fn friendlier_io_error_catches_zh_tw_access_denied() {
        let raw = "IoError: 存取被拒。 (os error 5)";
        let friendly = friendlier_io_error(raw);
        assert!(friendly.contains("admin-only"), "got: {friendly}");
        assert!(friendly.contains("手動下載"), "got: {friendly}");
    }

    #[test]
    fn friendlier_io_error_passes_through_unrelated() {
        let raw = "fetch releases: timeout";
        let friendly = friendlier_io_error(raw);
        assert!(friendly.starts_with("update:"), "got: {friendly}");
        assert!(friendly.contains("timeout"));
    }

    #[test]
    fn check_binary_writable_works_in_test_target() {
        check_binary_writable().expect("dev-mode target/ should be writable");
    }
}
