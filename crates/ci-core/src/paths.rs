//! Repo-relative path normalization — the one place a filesystem path becomes the
//! POSIX, repo-relative string form that index artifacts key on (SCIP document paths,
//! [`ImportGraph`](crate::ImportGraph) keys, fresh-overlay keys).

use std::path::Path;

/// Normalize a (possibly absolute) `path` to the repo-relative POSIX form used as an
/// index key. An absolute path under `root` is stripped to its remainder; a relative
/// path passes through as-is; an absolute path *outside* `root` passes through
/// unchanged (there is no meaningful relative form for it). Separators are always
/// normalized to `/`.
pub fn rel_path(root: &Path, path: &Path) -> String {
    let p = if path.is_absolute() { path.strip_prefix(root).unwrap_or(path) } else { path };
    p.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn absolute_under_root_becomes_relative() {
        let root = PathBuf::from("/repo");
        assert_eq!(rel_path(&root, &root.join("src").join("lib.rs")), "src/lib.rs");
    }

    #[test]
    fn already_relative_passes_through() {
        let root = PathBuf::from("/repo");
        assert_eq!(rel_path(&root, Path::new("src/lib.rs")), "src/lib.rs");
    }

    #[test]
    fn backslash_separators_normalize_to_posix() {
        let root = PathBuf::from("/repo");
        // A pre-joined string with `\` separators (as SCIP indexers emit on Windows).
        assert_eq!(rel_path(&root, Path::new("src\\a\\lib.rs")), "src/a/lib.rs");
        // The same, arriving absolute under the root.
        assert_eq!(rel_path(&root, Path::new("/repo/src\\lib.rs")), "src/lib.rs");
    }

    #[test]
    fn absolute_outside_root_passes_through_unchanged() {
        let root = PathBuf::from("/repo");
        assert_eq!(rel_path(&root, Path::new("/elsewhere/file.rs")), "/elsewhere/file.rs");
    }
}
