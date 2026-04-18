// Build-time environment injection for the `diagnose` MCP tool.
//
// Exposes:
//   env!("SIRIN_GIT_COMMIT")   — short SHA of HEAD at build time
//                                 (e.g. "37cfaf5")  — falls back to "unknown"
//                                 if `git` is unavailable or not a git repo.
//   env!("SIRIN_BUILD_DATE")   — RFC-3339 timestamp of the build host's clock
//                                 at compile time (UTC).

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    // Re-run on HEAD movement so the embedded SHA stays current.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    let commit = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=SIRIN_GIT_COMMIT={commit}");

    // Compose RFC-3339 from epoch seconds — avoids pulling chrono into the
    // build script (it's already a runtime dep, but keeping build.rs lean).
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Minimal RFC-3339 (UTC): YYYY-MM-DDTHH:MM:SSZ.  We use the epoch seconds
    // directly here; the runtime side formats it with chrono on read.
    println!("cargo:rustc-env=SIRIN_BUILD_EPOCH={now}");
}
