//! Filesystem helpers used by file_write / file_patch / file_list tools.
//!
//! - `safe_project_path` — path-traversal guard; ensures targets stay under
//!   `SIRIN_PROJECT_ROOT` (or cwd), and transparently resolves `foo.rs` to
//!   `foo/mod.rs` when that's how the project is actually laid out.
//! - `normalize_path` — resolves `.` / `..` without requiring the path to exist.
//! - `list_directory_tree` / `walk_dir` — recursive directory listing for
//!   `file_list`, skipping common noise (`target/`, `.git/`, etc.).

use std::path::{Component, Path, PathBuf};

/// Return the canonical absolute path for `path`, ensuring it is within the
/// project root (`SIRIN_PROJECT_ROOT` env var or `cwd`).
pub(super) fn safe_project_path(path: &str) -> Result<PathBuf, String> {
    let root = std::env::var("SIRIN_PROJECT_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "Cannot determine project root".to_string())?;

    let requested = root.join(path);

    // We can't canonicalize a path that doesn't exist yet, so normalize manually.
    let mut normalized = normalize_path(&requested);

    // Rust module convenience: if the agent guesses `foo.rs` but the project
    // actually uses `foo/mod.rs`, transparently resolve to the existing file.
    if !normalized.exists() && normalized.extension().and_then(|ext| ext.to_str()) == Some("rs") {
        let mod_candidate = normalized.with_extension("").join("mod.rs");
        if mod_candidate.is_file() {
            normalized = mod_candidate;
        }
    }

    let root_canon = std::fs::canonicalize(&root).unwrap_or(root.clone());
    let norm_canon = if normalized.exists() {
        std::fs::canonicalize(&normalized).unwrap_or(normalized.clone())
    } else {
        // For new files, canonicalize the parent and re-append the filename.
        let parent = normalized.parent().unwrap_or(&normalized);
        let parent_canon = if parent.exists() {
            std::fs::canonicalize(parent).unwrap_or(parent.to_path_buf())
        } else {
            parent.to_path_buf()
        };
        parent_canon.join(normalized.file_name().unwrap_or_default())
    };

    if !norm_canon.starts_with(&root_canon) {
        return Err(format!(
            "Path `{path}` resolves outside project root `{}`",
            root_canon.display()
        ));
    }
    Ok(normalized)
}

/// Normalize a path by resolving `.` and `..` components without requiring the
/// path to exist on disk (unlike `std::fs::canonicalize`).
pub(super) fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                components.pop();
            }
            Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Recursively list files under `dir` up to `max_depth` levels, returning
/// relative paths (from `dir`).  Skips common noise directories.
pub(super) fn list_directory_tree(dir: &str, max_depth: usize) -> Result<Vec<String>, String> {
    let root = Path::new(dir);
    if !root.exists() {
        return Err(format!("Directory not found: {dir}"));
    }
    let mut result = Vec::new();
    walk_dir(root, root, 0, max_depth, &mut result);
    result.sort();
    Ok(result)
}

fn walk_dir(
    root: &Path,
    current: &Path,
    depth: usize,
    max_depth: usize,
    result: &mut Vec<String>,
) {
    if depth > max_depth {
        return;
    }
    let skip_dirs = [
        ".git",
        "target",
        "node_modules",
        ".next",
        "dist",
        "__pycache__",
        ".cargo",
    ];
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') && depth == 0 {
            // Skip hidden top-level dirs except showing that they exist.
        }
        if path.is_dir() {
            if skip_dirs.contains(&name_str.as_ref()) {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                result.push(format!("{}/", rel.display()));
            }
            walk_dir(root, &path, depth + 1, max_depth, result);
        } else if let Ok(rel) = path.strip_prefix(root) {
            result.push(rel.display().to_string());
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_project_path_rejects_path_traversal() {
        let result = safe_project_path("../../etc/passwd");
        assert!(result.is_err(), "path traversal should be rejected, got: {:?}", result);

        let result2 = safe_project_path("../../../Windows/System32/config/sam");
        assert!(result2.is_err(), "deep traversal should be rejected, got: {:?}", result2);
    }

    #[test]
    fn normalize_path_collapses_dotdot() {
        let p = PathBuf::from("/tmp/foo/../bar");
        let norm = normalize_path(&p);
        assert_eq!(norm, PathBuf::from("/tmp/bar"));
    }

    #[test]
    fn safe_project_path_resolves_rust_module_to_mod_rs() {
        let path = safe_project_path("src/telegram.rs")
            .expect("should resolve Rust module path to the existing mod.rs file");
        assert!(
            path.ends_with(Path::new("src/telegram/mod.rs")),
            "unexpected resolved path: {}",
            path.display()
        );
    }
}
