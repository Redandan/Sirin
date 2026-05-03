//! Developer-experience drift tests (Issue #244).
//!
//! This module exists only for its `#[cfg(test)]` block.  The two tests
//! below walk the repo at test time:
//!
//! 1. `env_vars_documented` — scans `src/` for `std::env::var(...)` calls
//!    and verifies every var name appears in `.env.example`.  Catches the
//!    case where a feature lands but the example file isn't updated.
//!
//! 2. `scripts_index` — walks `scripts/` and verifies every `.sh` /
//!    `.ps1` is mentioned in `scripts/README.md`.  Catches new helper
//!    scripts that ship without a one-line description.
//!
//! Both tests are skipped silently when run from a sub-crate or
//! partial-checkout build where the expected files don't exist (e.g.
//! `cargo publish --dry-run` from a packaged tarball).

#![allow(dead_code)]

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    /// Curated list of env vars that are intentionally NOT in `.env.example`:
    /// - OS-built-in vars (HOME, APPDATA, etc.) Sirin reads but doesn't
    ///   document because the OS sets them.
    /// - Vars that only matter inside test code paths.
    fn excluded_env_vars() -> BTreeSet<&'static str> {
        [
            // OS-provided
            "HOME", "APPDATA", "LOCALAPPDATA", "USERPROFILE", "XDG_DATA_HOME",
        ].into_iter().collect()
    }

    /// Walk a directory recursively and call `cb(path, content)` for every
    /// regular file matching `ext`.  Silent on I/O errors.  Uses `dyn FnMut`
    /// to avoid monomorphization-recursion blow up — each recursive call
    /// would otherwise add a `&mut` layer to the generic type.
    fn walk_dir(root: &Path, ext: &str, cb: &mut dyn FnMut(&Path, &str)) {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_dir(&path, ext, cb);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some(ext) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                cb(&path, &content);
            }
        }
    }

    /// Issue #244 — every `std::env::var(...)` callsite under `src/` must
    /// appear in `.env.example` (or be in the OS-provided exclude list).
    #[test]
    fn env_vars_documented() {
        let src = Path::new("src");
        if !src.exists() { return; }

        let env_example = match std::fs::read_to_string(".env.example") {
            Ok(s) => s,
            Err(_) => return,
        };

        // Collect every env var name appearing in env::var calls.
        let mut found: BTreeSet<String> = BTreeSet::new();
        let pattern = "env::var(\"";
        walk_dir(src, "rs", &mut |_path, content| {
            let mut rest = content;
            while let Some(idx) = rest.find(pattern) {
                let after = &rest[idx + pattern.len()..];
                if let Some(end) = after.find('"') {
                    found.insert(after[..end].to_string());
                    rest = &after[end + 1..];
                } else {
                    break;
                }
            }
        });

        let excluded = excluded_env_vars();
        let undocumented: Vec<String> = found.into_iter()
            .filter(|name| !excluded.contains(name.as_str()))
            .filter(|name| !env_example.contains(name))
            .collect();

        if !undocumented.is_empty() {
            panic!(
                "{} env var(s) in src/ are not documented in .env.example:\n{}\n\n\
                 Add them to .env.example with a comment, or extend \
                 `excluded_env_vars()` in src/devex.rs if they really shouldn't \
                 be exposed (OS-builtin, test-only, etc.).",
                undocumented.len(),
                undocumented.iter().map(|n| format!("  - {n}"))
                    .collect::<Vec<_>>().join("\n"),
            );
        }
    }

    /// Issue #244 — every script under `scripts/` must be mentioned in
    /// `scripts/README.md` so a new contributor can grok them at a glance.
    #[test]
    fn scripts_index() {
        let dir = Path::new("scripts");
        if !dir.exists() { return; }

        let readme = match std::fs::read_to_string("scripts/README.md") {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut undocumented: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read scripts/").flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str());
            if !matches!(ext, Some("sh") | Some("ps1")) {
                continue;
            }
            // .example files mirror an installed script — describe in the
            // README under the canonical name (e.g. fetch-handoff.sh.example).
            let filename = path.file_name()
                .and_then(|s| s.to_str())
                .map(String::from)
                .unwrap_or_default();
            if !readme.contains(&filename) {
                undocumented.push(filename);
            }
        }
        if !undocumented.is_empty() {
            panic!(
                "{} script(s) under scripts/ are not mentioned in scripts/README.md:\n{}\n\n\
                 Add a row to the table in scripts/README.md describing what each does \
                 and when to run it.",
                undocumented.len(),
                undocumented.iter().map(|n| format!("  - {n}"))
                    .collect::<Vec<_>>().join("\n"),
            );
        }
    }
}
