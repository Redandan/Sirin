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
