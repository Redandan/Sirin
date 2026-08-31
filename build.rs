// Build-time environment injection for the `diagnose` MCP tool.
//
// Exposes:
//   env!("SIRIN_GIT_COMMIT")   — short SHA of HEAD at build time
//                                 (e.g. "37cfaf5")  — falls back to "unknown"
//                                 if `git` is unavailable or not a git repo.
//   env!("SIRIN_GIT_DIRTY")    — true when tracked or untracked source-tree
//                                 changes were present at build time; false
//                                 when clean; unknown when git is unavailable.
//   env!("SIRIN_BUILD_DATE")   — RFC-3339 timestamp of the build host's clock
//                                 at compile time (UTC).

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git_output(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=web");
    println!("cargo:rerun-if-changed=config");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=build.rs");
    // Re-run on HEAD movement so the embedded SHA stays current.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    let commit = git_output(&["rev-parse", "--short=7", "HEAD"])
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=SIRIN_GIT_COMMIT={commit}");

    let dirty = git_output(&["status", "--porcelain=v1", "--untracked-files=normal"])
        .map(|status| if status.is_empty() { "false" } else { "true" })
        .unwrap_or("unknown");
    println!("cargo:rustc-env=SIRIN_GIT_DIRTY={dirty}");

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
