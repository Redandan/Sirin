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
const BIN:   &str = "sirin";

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the current version string from Cargo.toml.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
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
/// Downloads the new binary, replaces self, sets `RestartRequired`.
pub fn apply_update(version: &str) -> Result<(), String> {
    set_status(UpdateStatus::Applying);

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
        .map_err(|e| format!("update: {e}"));

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
