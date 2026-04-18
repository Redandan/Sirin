//! Auto-update: checks GitHub Releases for a newer version, downloads
//! and replaces the binary, then signals the UI to prompt a restart.
//!
//! ## Flow
//! 1. `spawn_check()` — fires once on startup, background thread.
//!    Puts result into `UPDATE_STATE`.
//! 2. UI polls `pending_update()` each frame — shows banner if Some.
//! 3. User clicks "Update" → `apply_update()` — downloads + replaces binary.
//!    Returns `Ok(())` on success; caller shows "restart to apply" message.
//!
//! ## GitHub release format expected
//! Tag: `v0.2.0`
//! Asset: `sirin-windows-x86_64.zip` containing `sirin.exe`
//!
//! ## Dev behaviour
//! When built without `SIRIN_GITHUB_REPO` env the updater is a no-op so
//! local dev builds don't check for updates.

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

// Only used by the non-Windows self_update flow.  Kept conditional so
// Windows release builds don't emit a dead_code warning.
#[cfg(not(target_os = "windows"))]
const BIN: &str = "sirin";

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the current version string from Cargo.toml.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// URL to the GitHub Releases "latest" page — used by the UI as an escape hatch
/// when in-app self-update fails (typically because Sirin was installed under
/// `C:\Program Files\Sirin\` and replacing the binary needs admin).
pub fn release_page_url() -> String {
    format!("https://github.com/{OWNER}/{REPO}/releases/latest")
}

/// Best-effort check whether we can actually replace the running binary.
///
/// `self_update` does an in-place rename, which fails with `os error 5`
/// ("Access is denied" / 「存取被拒」) when:
///   - The user lacks write permission to the binary's directory
///   - Antivirus has the .exe locked
///
/// We catch case 1 by attempting to create + delete a tiny probe file in the
/// binary's parent directory.  Returns `Err(reason)` when the location isn't
/// writable so callers can short-circuit with a clearer message.
///
/// Windows uses the installer-download flow instead (admin elevation via UAC),
/// so this check is only wired up for macOS / Linux + the unit test.
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
            "Sirin 安裝在 {} — 自動更新需要該資料夾的寫入權限（通常是 admin）。\
             失敗原因：{}",
            dir.display(),
            e
        )),
    }
}

/// Spawn a background thread that checks for updates once.
/// Non-blocking — call at startup, poll `get_status()` in the UI loop.
pub fn spawn_check() {
    // Skip if no GitHub token / CI flag set — avoids noise in dev mode
    // (uncomment the env-guard if you want to gate on a token)
    set_status(UpdateStatus::Checking);
    std::thread::spawn(|| {
        match check_once() {
            Ok(Some(info)) => set_status(UpdateStatus::Available(info.version)),
            Ok(None)        => set_status(UpdateStatus::UpToDate),
            Err(e)          => set_status(UpdateStatus::CheckFailed(e)),
        }
    });
}

/// Blocking — call from a background thread / tokio spawn_blocking.
///
/// ## Windows strategy (v0.4.1+)
///
/// Downloads `SirinSetup-{version}.exe` from GitHub Releases and spawns it
/// with `/SILENT /SUPPRESSMSGBOXES`.  Inno Setup binaries declare
/// `requestedExecutionLevel=requireAdministrator` in their manifest, so
/// Windows automatically shows a UAC prompt — the user clicks "Yes" once
/// and the installer takes over with admin rights.  This **completely
/// bypasses the `C:\Program Files\Sirin\` write-denied problem** that broke
/// the previous direct-replace flow (v0.2.0–v0.4.0).
///
/// After spawning the installer, Sirin self-exits in 2 seconds so the
/// installer can replace `sirin.exe`.  The installer's `[Run]` section
/// auto-launches the new Sirin when finished.
///
/// ## Non-Windows strategy
///
/// Falls back to `self_update`'s direct binary-replace flow.  macOS / Linux
/// don't have the admin-folder issue when Sirin lives under `~/.local/bin/`
/// or similar.
pub fn apply_update(version: &str) -> Result<(), String> {
    set_status(UpdateStatus::Applying);

    // ── Windows: download installer + UAC elevation ──────────────────────────
    #[cfg(target_os = "windows")]
    {
        match download_and_run_installer(version) {
            Ok(()) => {
                set_status(UpdateStatus::RestartRequired);
                // Schedule self-exit so installer can write to sirin.exe.
                // 2s gives the spawned installer time to read its own bytes
                // off disk before we vanish.
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

    // ── Non-Windows: keep direct-binary-replace via self_update ──────────────
    #[cfg(not(target_os = "windows"))]
    {
        if let Err(reason) = check_binary_writable() {
            let msg = format!("{reason}\n建議：從 GitHub Releases 手動下載新版本。");
            set_status(UpdateStatus::ApplyFailed(msg.clone()));
            return Err(msg);
        }

        let result = self_update::backends::github::Update::configure()
            .repo_owner(OWNER)
            .repo_name(REPO)
            .bin_name(BIN)
            .target_version_tag(&format!("v{version}"))
            .current_version(current_version())
            .no_confirm(true)
            .build()
            .map_err(|e| format!("build updater: {e}"))?
            .update()
            .map_err(|e| friendlier_io_error(&e.to_string()));

        match result {
            Ok(_) => {
                set_status(UpdateStatus::RestartRequired);
                Ok(())
            }
            Err(e) => {
                set_status(UpdateStatus::ApplyFailed(e.clone()));
                Err(e)
            }
        }
    }
}

/// Download `SirinSetup-{version}.exe` from GitHub Releases and spawn it.
///
/// Returns `Ok(())` once the installer has been spawned (the actual install
/// happens in another process; we don't wait for it).
///
/// **The installer requires admin elevation** — Windows auto-prompts UAC
/// because Inno Setup bakes `requestedExecutionLevel=requireAdministrator`
/// into the .exe manifest.  No special handling needed on our end.
#[cfg(target_os = "windows")]
fn download_and_run_installer(version: &str) -> Result<(), String> {
    let url = format!(
        "https://github.com/{OWNER}/{REPO}/releases/download/v{version}/SirinSetup-{version}.exe"
    );

    // 6 MB installer over GitHub CDN — 120s gives slow connections headroom.
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let resp = client.get(&url)
        .send()
        .map_err(|e| format!("download installer: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "installer asset not found at {url} (HTTP {})",
            resp.status()
        ));
    }

    let bytes = resp.bytes()
        .map_err(|e| format!("read installer body: {e}"))?;

    // Save under stable name in %TEMP% so the user can find it later if
    // something goes wrong (e.g. UAC denied).
    let installer_path = std::env::temp_dir()
        .join(format!("SirinSetup-{version}.exe"));
    std::fs::write(&installer_path, &bytes)
        .map_err(|e| format!("write installer to {}: {e}", installer_path.display()))?;

    // /SILENT      — no wizard pages, but progress bar still shows
    // /SUPPRESSMSGBOXES — auto-confirm any dialogs (Replace? etc.)
    // Inno Setup's auto-elevation triggers UAC here.
    std::process::Command::new(&installer_path)
        .args(["/SILENT", "/SUPPRESSMSGBOXES"])
        .spawn()
        .map_err(|e| format!("spawn installer: {e}"))?;

    Ok(())
}

/// Convert raw `self_update` errors into something the user can act on.
/// Specifically catches Windows "Access is denied" / 「存取被拒」 so the UI
/// can suggest the manual-installer fallback.
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
    fn friendlier_io_error_catches_access_denied_english() {
        let raw = "IoError: Access is denied. (os error 5)";
        let friendly = friendlier_io_error(raw);
        assert!(friendly.contains("admin-only"), "got: {friendly}");
        assert!(friendly.contains("手動下載"), "got: {friendly}");
    }

    #[test]
    fn friendlier_io_error_catches_zh_tw_access_denied() {
        // The exact string the user reported.
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
        // Tests run from target/debug — should be writable, so this should
        // succeed.  If it fails, our pre-check would block legitimate dev-mode
        // updates too.
        check_binary_writable().expect("dev-mode target/ should be writable");
    }
}

// ── Internal ──────────────────────────────────────────────────────────────────

fn check_once() -> Result<Option<UpdateInfo>, String> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .build()
        .map_err(|e| format!("build release-list: {e}"))?
        .fetch()
        .map_err(|e| format!("fetch releases: {e}"))?;

    let latest = match releases.first() {
        Some(r) => r,
        None => return Ok(None),
    };

    let latest_ver = latest.version.trim_start_matches('v');
    if self_update::version::bump_is_greater(current_version(), latest_ver)
        .unwrap_or(false)
    {
        Ok(Some(UpdateInfo {
            version: latest_ver.to_string(),
            release_notes: latest.body.clone().unwrap_or_default(),
        }))
    } else {
        Ok(None)
    }
}
