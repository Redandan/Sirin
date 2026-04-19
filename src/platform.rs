//! Cross-platform data directory resolution.
//!
//! | OS      | Path                                      |
//! |---------|-------------------------------------------|
//! | Windows | `%LOCALAPPDATA%\Sirin`                    |
//! | macOS   | `~/Library/Application Support/Sirin`     |
//! | Linux   | `~/.local/share/sirin`                    |
//! | fallback| `data/`                                   |

use std::path::PathBuf;

/// Returns the platform-appropriate directory for Sirin's persistent data.
/// The directory is **not** guaranteed to exist; callers must `create_dir_all`.
pub fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(local).join("Sirin");
    }

    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Sirin");
    }

    #[cfg(target_os = "linux")]
    {
        // Respect XDG_DATA_HOME if set, otherwise ~/.local/share
        let base = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".local").join("share"))
                    .unwrap_or_else(|_| PathBuf::from("data"))
            });
        return base.join("sirin");
    }

    #[allow(unreachable_code)]
    PathBuf::from("data")
}

/// Returns the `config/` directory.
///
/// | Mode | Path |
/// |------|------|
/// | Production (installed) | `app_data_dir()/config` |
/// | Test builds (`cargo test`) | `./config` (repo-relative) |
///
/// Tests use the repo's `config/` so they can find their fixture YAML files
/// without requiring a pre-populated `%LOCALAPPDATA%\Sirin\config\`.
pub fn config_dir() -> PathBuf {
    #[cfg(test)]
    return PathBuf::from("config");

    #[cfg(not(test))]
    app_data_dir().join("config")
}

/// Returns `config_dir()/<rel>`.
///
/// Drop-in replacement for hard-coded `"config/foo.yaml"` literals:
/// ```ignore
/// std::fs::read_to_string(platform::config_path("persona.yaml"))
/// ```
pub fn config_path(rel: impl AsRef<std::path::Path>) -> PathBuf {
    config_dir().join(rel)
}

// ── Silent subprocess spawn (no console window flash on Windows) ─────────────
//
// Background:
//   On Windows, `std::process::Command::new()` (and tokio's variant) opens a
//   visible cmd.exe window for *every* subprocess.  Sirin spawns chrome,
//   claude, node, git, cargo etc. from many places — without this flag the
//   user sees a black box flicker every few seconds, especially during
//   startup (config_check probes `chrome --version`, `claude --version`).
//
// The `CREATE_NO_WINDOW = 0x08000000` flag on `CreateProcessW` suppresses
// the console.  On non-Windows targets the trait is a no-op.
//
// Usage:
//   use crate::platform::NoWindow;
//   Command::new("git").no_window().args(["status"]).output()?;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Extension trait — adds `.no_window()` to `Command` so subprocess spawns
/// do not flash a console window on Windows.  No-op on Unix.
///
/// Apply to **every** `Command::new(...)` in the codebase.  Even a 50ms
/// `git --version` probe pops a visible window if you forget it.
pub trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}

impl NoWindow for std::process::Command {
    #[cfg(windows)]
    fn no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(CREATE_NO_WINDOW)
    }
    #[cfg(not(windows))]
    fn no_window(&mut self) -> &mut Self { self }
}

impl NoWindow for tokio::process::Command {
    #[cfg(windows)]
    fn no_window(&mut self) -> &mut Self {
        self.creation_flags(CREATE_NO_WINDOW)
    }
    #[cfg(not(windows))]
    fn no_window(&mut self) -> &mut Self { self }
}
