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

/// The READ-layer twin of ci-edit's write jail (`ensure_within_root`): the in-root
/// relative form of `path`, or `None` when it escapes `root`. Providers gate their
/// `structure()` reads on this so an inspect/read path (or a node id like
/// `../x.py#sym`) can never read outside the registered workspace. Two layers,
/// matching the write side:
///   1. lexical — an absolute path outside `root` and any `..` that climbs above
///      root refuse (`a/../b` inside stays fine);
///   2. symlink — the nearest existing ancestor of the target must canonicalize
///      under root (a symlinked dir pointing out refuses). Best-effort: skipped
///      only if `root` itself can't canonicalize.
pub fn jailed_rel(root: &Path, path: &Path) -> Option<String> {
    use std::path::Component;
    let rel = if path.is_absolute() {
        match path.strip_prefix(root) {
            Ok(stripped) => stripped,
            Err(_) => return None, // absolute outside root: no in-root form
        }
    } else {
        path
    };
    let mut depth: i32 = 0;
    for comp in rel.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => return None,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
        }
    }
    if let Ok(root_c) = root.canonicalize() {
        let abs = root.join(rel);
        let mut probe = abs.as_path();
        loop {
            match probe.canonicalize() {
                Ok(resolved) => {
                    if !resolved.starts_with(&root_c) {
                        return None;
                    }
                    break;
                }
                Err(_) => match probe.parent() {
                    Some(parent) => probe = parent,
                    None => break,
                },
            }
        }
    }
    Some(rel.to_string_lossy().replace('\\', "/"))
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

    #[test]
    fn jailed_rel_accepts_in_root_forms() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").unwrap();
        assert_eq!(jailed_rel(root, Path::new("src/lib.rs")).as_deref(), Some("src/lib.rs"));
        // absolute under root strips; internal `..` that stays inside is fine
        assert_eq!(jailed_rel(root, &root.join("src/lib.rs")).as_deref(), Some("src/lib.rs"));
        assert_eq!(jailed_rel(root, Path::new("src/../src/lib.rs")).as_deref(), Some("src/../src/lib.rs"));
    }

    #[test]
    fn jailed_rel_refuses_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(dir.path().join("../outside.rs"), "fn x() {}\n").ok();
        assert_eq!(jailed_rel(root, Path::new("../outside.rs")), None);
        assert_eq!(jailed_rel(root, Path::new("src/../../outside.rs")), None);
        assert_eq!(jailed_rel(root, Path::new("/etc/hosts")), None);
    }

    #[cfg(unix)]
    #[test]
    fn jailed_rel_refuses_symlink_escapes() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "fn s() {}\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        assert_eq!(jailed_rel(root, Path::new("link/secret.rs")), None);
    }
}
