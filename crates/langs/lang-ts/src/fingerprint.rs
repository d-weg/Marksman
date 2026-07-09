//! Source fingerprint for the persisted `index.scip` — decides load-cached vs reindex at
//! startup. The machinery (content hashing, walk, drift detection, versioned atomic
//! load/store) is [`ci_core::fingerprint`]; this module holds only what is TypeScript's to
//! know: which files can change scip-typescript's output (`.ts*`/`.js*` sources,
//! `tsconfig*.json`, `package.json` + lockfiles), where the fingerprint lives, its format
//! version, and the pinned-tool marker. Staleness here is a correctness bug (a stale index
//! once sent a rename to "symbol not found" and the agent fell back to grep), so every
//! doubtful case — missing or unparsable fingerprint, version bump — reads as "changed" and
//! reindexes.
use std::path::{Path, PathBuf};

pub(crate) use ci_core::fingerprint::{augment_fingerprint, Fingerprint};

/// Bump when the fingerprint's inputs or format change; old files then read as stale.
/// v2: added the `//scip-typescript` tool marker + scip-document augmentation.
const FP_VERSION: u64 = 2;

/// Synthetic map key holding the pinned scip-typescript version — a tool bump must reindex
/// exactly like a source change would. `//` can never prefix a real repo-relative path.
const TOOL_KEY: &str = "//scip-typescript";

pub(crate) fn fingerprint_path(root: &Path) -> PathBuf {
    root.join(".marksman").join("index.scip.fingerprint.json")
}

/// Does this file feed scip-typescript's output? Sources (`.d.ts` included — it can't hurt),
/// `.js*` too (an `allowJs` project type-checks them), and the project config that shapes the
/// index: tsconfig variants and package.json/lockfiles (the proxy for dependency changes,
/// since node_modules itself is never walked).
fn is_input(name: &str) -> bool {
    const SRC_EXT: [&str; 8] = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];
    if SRC_EXT.iter().any(|e| name.ends_with(e)) {
        return true;
    }
    (name.starts_with("tsconfig") && name.ends_with(".json"))
        || matches!(name, "package.json" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml")
}

/// Hash every fingerprint input under `root` (gitignore-aware; hidden dirs and `node_modules`
/// always skipped, so a repo without a .gitignore can't drag thousands of dependency files in).
/// Includes the pinned scip-typescript version under [`TOOL_KEY`].
pub(crate) fn source_fingerprint(root: &Path) -> Fingerprint {
    let mut map = ci_core::fingerprint::source_fingerprint(
        root,
        &["node_modules"],
        |rel| rel.file_name().is_some_and(|n| is_input(&n.to_string_lossy())),
        &[],
    );
    map.insert(TOOL_KEY.into(), crate::SCIP_TS_VERSION.into());
    map
}

/// `None` when the source still matches `stored`; else a short human-readable reason (for the
/// "reindexing because …" startup log). Stored-only entries — the AUGMENTED files scip indexed
/// but the walk can't see — are re-hashed from disk; see [`ci_core::fingerprint::drift_reason`].
pub(crate) fn fingerprint_drift(root: &Path, stored: &Fingerprint, current: &Fingerprint) -> Option<String> {
    ci_core::fingerprint::drift_reason(root, stored, current)
}

pub(crate) fn load_fingerprint(path: &Path) -> Option<Fingerprint> {
    ci_core::fingerprint::load_fingerprint(path, FP_VERSION)
}

pub(crate) fn store_fingerprint(path: &Path, fp: &Fingerprint) -> std::io::Result<()> {
    ci_core::fingerprint::store_fingerprint(path, FP_VERSION, fp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn detects_content_structural_and_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "tsconfig.json", "{}");
        write(root, "src/a.ts", "export const x = 1;");
        write(root, "src/b.ts", "import { x } from './a.js';");
        write(root, "node_modules/dep/index.ts", "export {};"); // never fingerprinted
        write(root, "README.md", "# not an input");

        let base = source_fingerprint(root);
        assert!(base.contains_key("src/a.ts") && base.contains_key("tsconfig.json"));
        assert!(base.contains_key(TOOL_KEY), "tool marker always present");
        assert!(!base.keys().any(|k| k.contains("node_modules") || k.ends_with(".md")));

        // Unchanged source -> identical fingerprint, even after an mtime-only rewrite
        // (a git reset restores bytes, not mtimes — this is the cache-hit case).
        write(root, "src/a.ts", "export const x = 1;");
        assert_eq!(fingerprint_drift(root, &base, &source_fingerprint(root)), None);

        // Content edit (an import edge change is just this).
        write(root, "src/b.ts", "import { x } from './c.js';");
        assert!(fingerprint_drift(root, &base, &source_fingerprint(root)).unwrap().contains("changed"));
        write(root, "src/b.ts", "import { x } from './a.js';");

        // New file.
        write(root, "src/new.ts", "export {};");
        assert!(fingerprint_drift(root, &base, &source_fingerprint(root)).unwrap().contains("added"));

        // Moved file = one added + the old path gone.
        fs::rename(root.join("src/new.ts"), root.join("src/moved.ts")).unwrap();
        let diff = fingerprint_drift(root, &base, &source_fingerprint(root)).unwrap();
        assert!(diff.contains("added") && diff.contains("moved.ts"));
        fs::remove_file(root.join("src/moved.ts")).unwrap();

        // Deleting a fingerprinted file invalidates (stored-only entry, unreadable on disk).
        let with_b = source_fingerprint(root);
        fs::remove_file(root.join("src/b.ts")).unwrap();
        assert!(fingerprint_drift(root, &with_b, &source_fingerprint(root)).unwrap().contains("removed"));
        write(root, "src/b.ts", "import { x } from './a.js';");

        // tsconfig edit invalidates too.
        write(root, "tsconfig.json", r#"{"compilerOptions":{}}"#);
        assert!(fingerprint_drift(root, &base, &source_fingerprint(root)).is_some());
    }

    // AUGMENTED entries: files scip indexed but the walk can't see (hidden/gitignored sources a
    // tsconfig still includes) are stored with their hash at index time and re-checked from
    // disk — identical bytes stay fresh, an edit or deletion drifts.
    #[test]
    fn augmented_hidden_files_are_rechecked_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.ts", "export const x = 1;");
        write(root, ".gen/hidden.ts", "export const g = 1;"); // hidden dir: walk never sees it

        let mut stored = source_fingerprint(root);
        assert!(!stored.contains_key(".gen/hidden.ts"), "walk must not see the hidden file");
        augment_fingerprint(&mut stored, root, [".gen/hidden.ts".to_string()]);
        assert!(stored.contains_key(".gen/hidden.ts"), "augmentation hashes it from disk");

        // Unchanged hidden file -> fresh.
        assert_eq!(fingerprint_drift(root, &stored, &source_fingerprint(root)), None);
        // Edited hidden file -> drift.
        write(root, ".gen/hidden.ts", "export const g = 2;");
        assert!(fingerprint_drift(root, &stored, &source_fingerprint(root)).unwrap().contains(".gen/hidden.ts"));
        // Deleted hidden file -> drift.
        fs::remove_file(root.join(".gen/hidden.ts")).unwrap();
        assert!(fingerprint_drift(root, &stored, &source_fingerprint(root)).unwrap().contains("removed"));
    }

    #[test]
    fn roundtrips_and_rejects_bad_or_versioned_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.ts", "export const x = 1;");
        let fp = source_fingerprint(root);
        let path = fingerprint_path(root);

        store_fingerprint(&path, &fp).unwrap();
        assert_eq!(load_fingerprint(&path), Some(fp));

        // Corrupt file and wrong version both read as "no fingerprint" (-> reindex).
        fs::write(&path, "{ truncated").unwrap();
        assert_eq!(load_fingerprint(&path), None);
        fs::write(&path, r#"{"version":999,"files":{}}"#).unwrap();
        assert_eq!(load_fingerprint(&path), None);
    }
}
