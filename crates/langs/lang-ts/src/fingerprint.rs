//! Source fingerprint for the persisted `index.scip` — decides load-cached vs reindex at
//! startup. A fingerprint is the full map of every file that can change scip-typescript's
//! output (`.ts*`/`.js*` sources, `tsconfig*.json`, `package.json` + lockfiles) to a hash of
//! its CONTENT. Comparing whole maps catches everything the index depends on: edited files
//! (imports included), and added/removed/moved files show up as key changes. Content hashes,
//! not mtimes, on purpose: a `git checkout`/reset rewrites mtimes but not bytes, and must
//! still hit the cache. Staleness here is a correctness bug (a stale index once sent a rename
//! to "symbol not found" and the agent fell back to grep), so every doubtful case — missing
//! or unparsable fingerprint, version bump — reads as "changed" and reindexes.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bump when the fingerprint's inputs or format change; old files then read as stale.
/// v2: added the `//scip-typescript` tool marker + scip-document augmentation.
const FP_VERSION: u64 = 2;

/// Synthetic map key holding the pinned scip-typescript version — a tool bump must reindex
/// exactly like a source change would. `//` can never prefix a real repo-relative path.
const TOOL_KEY: &str = "//scip-typescript";

pub(crate) type Fingerprint = BTreeMap<String, String>;

pub(crate) fn fingerprint_path(root: &Path) -> PathBuf {
    root.join(".marksman").join("index.scip.fingerprint.json")
}

/// FNV-1a 64-bit. Non-cryptographic is fine: this detects accidental drift, not tampering.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
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

/// Content hash of one file, in the fingerprint's format.
pub(crate) fn hash_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| format!("{:016x}", fnv1a(&b)))
}

/// Hash every fingerprint input under `root` (gitignore-aware; hidden dirs and `node_modules`
/// always skipped, so a repo without a .gitignore can't drag thousands of dependency files in).
/// Includes the pinned scip-typescript version under [`TOOL_KEY`].
pub(crate) fn source_fingerprint(root: &Path) -> Fingerprint {
    let mut map = Fingerprint::new();
    map.insert(TOOL_KEY.into(), crate::SCIP_TS_VERSION.into());
    let walker = ignore::WalkBuilder::new(root)
        .filter_entry(|e| e.depth() == 0 || e.file_name().to_string_lossy() != "node_modules")
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if !is_input(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let Ok(bytes) = std::fs::read(entry.path()) else { continue };
        map.insert(rel.to_string_lossy().replace('\\', "/"), format!("{:016x}", fnv1a(&bytes)));
    }
    map
}

/// `None` when the source still matches `stored`; else a short human-readable reason (for the
/// "reindexing because …" startup log). Two-sided:
/// - every entry the CURRENT walk sees must match `stored` (new/changed walk-visible files,
///   and the tool-version marker);
/// - every `stored` entry the walk did NOT see is re-hashed from disk. These are the AUGMENTED
///   files — indexed by scip but invisible to the walk (gitignored/hidden sources a tsconfig
///   still includes) — plus walk-visible files that were deleted (their read fails → drift).
///   A file that merely became walk-invisible with identical bytes stays fresh.
pub(crate) fn fingerprint_drift(root: &Path, stored: &Fingerprint, current: &Fingerprint) -> Option<String> {
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut missing = Vec::new();
    for (k, h) in current {
        match stored.get(k) {
            None => added.push(k.clone()),
            Some(s) if s != h => changed.push(k.clone()),
            _ => {}
        }
    }
    for (k, s) in stored {
        if current.contains_key(k) || k.starts_with("//") {
            continue; // compared above; a stored tool marker absent from current is unreachable
        }
        match hash_file(&root.join(k)) {
            Some(h) if h == *s => {} // unchanged, just not walk-visible
            Some(_) => changed.push(k.clone()),
            None => missing.push(k.clone()),
        }
    }
    let mut parts = Vec::new();
    for (label, set) in [("added", &added), ("changed", &changed), ("removed/unreadable", &missing)] {
        if let Some(first) = set.first() {
            parts.push(format!("{} file(s) {label} (e.g. {first})", set.len()));
        }
    }
    if parts.is_empty() { None } else { Some(parts.join(", ")) }
}

pub(crate) fn load_fingerprint(path: &Path) -> Option<Fingerprint> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if v["version"].as_u64() != Some(FP_VERSION) {
        return None;
    }
    v["files"].as_object()?.iter().map(|(k, h)| Some((k.clone(), h.as_str()?.to_string()))).collect()
}

/// Persist atomically (tmp + rename) so a crash mid-write leaves either the old fingerprint or
/// none — both of which read as "reindex", never as a false match.
pub(crate) fn store_fingerprint(path: &Path, fp: &Fingerprint) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::json!({ "version": FP_VERSION, "files": fp }).to_string())?;
    std::fs::rename(&tmp, path)
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
        stored.insert(".gen/hidden.ts".into(), hash_file(&root.join(".gen/hidden.ts")).unwrap());

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
