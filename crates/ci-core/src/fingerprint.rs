//! Content fingerprints for persisted index artifacts — they decide load-cached vs reindex at
//! startup, and which files' cached data can no longer be trusted. A fingerprint is the full
//! map of every file that can change an indexer's output to a hash of its CONTENT. Comparing
//! whole maps catches everything the artifact depends on: edited files (imports included), and
//! added/removed/moved files show up as key changes. Content hashes, not mtimes, on purpose: a
//! `git checkout`/reset rewrites mtimes but not bytes, and must still hit the cache. Staleness
//! is a correctness bug (a stale index once sent a rename to "symbol not found" and the agent
//! fell back to grep), so every doubtful case — missing or unparsable fingerprint, version
//! bump — reads as "changed" and reindexes.
//!
//! The per-language knowledge stays in the provider: which files feed its indexer
//! (`is_input`), where its fingerprint file lives, and its format version constant. Keys
//! starting with `//` are synthetic markers (e.g. a pinned indexer version — a tool bump must
//! invalidate exactly like a source change would); `//` can never prefix a real repo-relative
//! path, so markers are compared map-to-map and never re-read from disk.
use std::collections::BTreeMap;
use std::path::Path;

/// Repo-relative path (posix separators) → 16-hex-digit content hash.
pub type Fingerprint = BTreeMap<String, String>;

/// FNV-1a 64-bit. Non-cryptographic is fine: this detects accidental drift, not tampering.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Content hash of one file, in the fingerprint's format.
pub fn hash_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| format!("{:016x}", fnv1a(&b)))
}

/// Hash every fingerprint input under `root`. The walk is gitignore-aware with hidden dirs
/// always skipped, and never descends into directories named in `skip_dirs` — pruned at the
/// directory, not filtered per file, so a repo without a .gitignore can't drag thousands of
/// dependency files in. `is_input` sees each regular file's repo-relative path; `extra_files`
/// are augmented in on top (see [`augment_fingerprint`]) for inputs the walk can't see.
pub fn source_fingerprint(
    root: &Path,
    skip_dirs: &[&str],
    is_input: impl Fn(&Path) -> bool,
    extra_files: &[String],
) -> Fingerprint {
    let mut map = Fingerprint::new();
    let skip: Vec<String> = skip_dirs.iter().map(|s| s.to_string()).collect();
    let walker = ignore::WalkBuilder::new(root)
        .filter_entry(move |e| {
            e.depth() == 0 || !skip.iter().any(|d| e.file_name().to_string_lossy() == *d)
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        if !is_input(rel) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else { continue };
        map.insert(rel.to_string_lossy().replace('\\', "/"), format!("{:016x}", fnv1a(&bytes)));
    }
    augment_fingerprint(&mut map, root, extra_files.iter().cloned());
    map
}

/// Hash `files` (repo-relative) into `fp` where absent — the AUGMENTED entries: files the
/// indexer consumed that the fingerprint walk can't see (gitignored/hidden sources it still
/// includes). Hashed at call time — AFTER the indexer ran, when the caller learns of them from
/// the artifact itself — so a conservative pre-run fingerprint's guarantee narrows to just
/// these files. An unreadable path contributes no entry.
pub fn augment_fingerprint(fp: &mut Fingerprint, root: &Path, files: impl IntoIterator<Item = String>) {
    for f in files {
        if let std::collections::btree_map::Entry::Vacant(slot) = fp.entry(f) {
            if let Some(h) = hash_file(&root.join(slot.key())) {
                slot.insert(h);
            }
        }
    }
}

/// `None` unless `path` parses and carries exactly `version` — corrupt and version-bumped
/// fingerprints both read as "no fingerprint" (→ reindex).
pub fn load_fingerprint(path: &Path, version: u64) -> Option<Fingerprint> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if v["version"].as_u64() != Some(version) {
        return None;
    }
    v["files"].as_object()?.iter().map(|(k, h)| Some((k.clone(), h.as_str()?.to_string()))).collect()
}

/// Persist atomically (tmp + rename) so a crash mid-write leaves either the old fingerprint or
/// none — both of which read as "reindex", never as a false match.
pub fn store_fingerprint(path: &Path, version: u64, fp: &Fingerprint) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::json!({ "version": version, "files": fp }).to_string())?;
    std::fs::rename(&tmp, path)
}

/// The one diff walk both drift shapes ([`drift_reason`], [`drifted_files`]) read. Two-sided:
/// - every entry the CURRENT walk sees must match `stored` (new/changed walk-visible files,
///   and the `//` markers);
/// - every `stored` entry the walk did NOT see is re-hashed from disk. These are the augmented
///   files — indexed but invisible to the walk — plus walk-visible files that were deleted
///   (their read fails → drift). A file that merely became walk-invisible with identical bytes
///   stays fresh.
struct Diff {
    added: Vec<String>,
    changed: Vec<String>,
    missing: Vec<String>,
}

fn diff(root: &Path, stored: &Fingerprint, current: &Fingerprint) -> Diff {
    let mut d = Diff { added: Vec::new(), changed: Vec::new(), missing: Vec::new() };
    for (k, h) in current {
        match stored.get(k) {
            None => d.added.push(k.clone()),
            Some(s) if s != h => d.changed.push(k.clone()),
            _ => {}
        }
    }
    for (k, s) in stored {
        if current.contains_key(k) || k.starts_with("//") {
            continue; // compared above; a stored marker absent from current is unreachable
        }
        match hash_file(&root.join(k)) {
            Some(h) if h == *s => {} // unchanged, just not walk-visible
            Some(_) => d.changed.push(k.clone()),
            None => d.missing.push(k.clone()),
        }
    }
    d
}

/// `None` when the source still matches `stored`; else a short human-readable reason (for the
/// "reindexing because …" startup log). The whole-index drift shape: callers that separate
/// "no usable fingerprint" from "drifted" load the stored map themselves
/// ([`load_fingerprint`]) so each case gets its own message.
pub fn drift_reason(root: &Path, stored: &Fingerprint, current: &Fingerprint) -> Option<String> {
    let d = diff(root, stored, current);
    let mut parts = Vec::new();
    for (label, set) in
        [("added", &d.added), ("changed", &d.changed), ("removed/unreadable", &d.missing)]
    {
        if let Some(first) = set.first() {
            parts.push(format!("{} file(s) {label} (e.g. {first})", set.len()));
        }
    }
    if parts.is_empty() { None } else { Some(parts.join(", ")) }
}

/// The per-file drift shape: every file that changed/appeared/disappeared since the
/// fingerprint at `fp_path` was stored — the set whose cached data can no longer be trusted.
/// `None` = no usable fingerprint there (treat EVERYTHING as drifted: pre-fingerprint caches
/// never get silently blessed); `Some(vec![])` = fully fresh.
pub fn drifted_files(
    root: &Path,
    fp_path: &Path,
    version: u64,
    current: &Fingerprint,
) -> Option<Vec<String>> {
    let stored = load_fingerprint(fp_path, version)?;
    let d = diff(root, &stored, current);
    let mut out = [d.added, d.changed, d.missing].concat();
    out.sort();
    Some(out)
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

    fn rs_input(rel: &Path) -> bool {
        rel.extension().is_some_and(|e| e == "rs")
    }

    #[test]
    fn walk_respects_skip_dirs_hidden_dirs_and_the_predicate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, "vendor/dep/lib.rs", "fn v() {}"); // pruned via skip_dirs
        write(root, ".hidden/h.rs", "fn h() {}"); // hidden dir: walk never sees it
        write(root, "README.md", "# not an input");

        let fp = source_fingerprint(root, &["vendor"], rs_input, &[]);
        assert_eq!(fp.keys().collect::<Vec<_>>(), vec!["src/a.rs"]);
    }

    #[test]
    fn extra_files_augment_without_clobbering_walked_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, ".gen/g.rs", "fn g() {}");

        let base = source_fingerprint(root, &[], rs_input, &[]);
        let extras =
            vec![".gen/g.rs".to_string(), "src/a.rs".to_string(), "no/such/file.rs".to_string()];
        let fp = source_fingerprint(root, &[], rs_input, &extras);
        assert_eq!(fp.get(".gen/g.rs"), hash_file(&root.join(".gen/g.rs")).as_ref());
        assert_eq!(fp.get("src/a.rs"), base.get("src/a.rs"), "walked entry wins over the extra");
        assert!(!fp.contains_key("no/such/file.rs"), "unreadable extras contribute nothing");
    }

    #[test]
    fn both_drift_shapes_read_the_same_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, "src/b.rs", "fn b() {}");
        let fp_path = root.join("fp.json");
        let stored = source_fingerprint(root, &[], rs_input, &[]);
        store_fingerprint(&fp_path, 1, &stored).unwrap();

        // Fresh: reason None, per-file list empty.
        let current = source_fingerprint(root, &[], rs_input, &[]);
        assert_eq!(drift_reason(root, &stored, &current), None);
        assert_eq!(drifted_files(root, &fp_path, 1, &current), Some(vec![]));

        // Changed + added + deleted land in both shapes.
        write(root, "src/a.rs", "fn a() { /* edited */ }");
        write(root, "src/new.rs", "fn n() {}");
        fs::remove_file(root.join("src/b.rs")).unwrap();
        let current = source_fingerprint(root, &[], rs_input, &[]);
        let reason = drift_reason(root, &stored, &current).unwrap();
        assert!(reason.contains("added") && reason.contains("changed") && reason.contains("removed"));
        assert_eq!(
            drifted_files(root, &fp_path, 1, &current),
            Some(vec!["src/a.rs".into(), "src/b.rs".into(), "src/new.rs".into()])
        );
    }

    #[test]
    fn stored_only_entries_are_rehashed_from_disk_and_markers_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, ".gen/g.rs", "fn g() {}"); // walk-invisible (hidden dir)

        let mut stored = source_fingerprint(root, &[], rs_input, &[]);
        stored.insert("//tool".into(), "1.2.3".into());
        augment_fingerprint(&mut stored, root, [".gen/g.rs".to_string()]);

        // Identical bytes off-walk stay fresh; the marker is never read from disk.
        let mut current = source_fingerprint(root, &[], rs_input, &[]);
        current.insert("//tool".into(), "1.2.3".into());
        assert_eq!(drift_reason(root, &stored, &current), None);

        // An off-walk edit drifts; a marker bump drifts.
        write(root, ".gen/g.rs", "fn g() { /* edited */ }");
        assert!(drift_reason(root, &stored, &current).unwrap().contains(".gen/g.rs"));
        current.insert("//tool".into(), "9.9.9".into());
        assert!(drift_reason(root, &stored, &current).unwrap().contains("//tool"));
    }

    #[test]
    fn versioned_load_rejects_corrupt_missing_and_bumped_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "fn a() {}");
        let fp_path = root.join("fp.json");
        let fp = source_fingerprint(root, &[], rs_input, &[]);
        let current = fp.clone();

        assert_eq!(load_fingerprint(&fp_path, 1), None, "missing file");
        assert_eq!(drifted_files(root, &fp_path, 1, &current), None);

        store_fingerprint(&fp_path, 1, &fp).unwrap();
        assert_eq!(load_fingerprint(&fp_path, 1), Some(fp.clone()));
        assert_eq!(load_fingerprint(&fp_path, 2), None, "version bump reads as no fingerprint");
        assert_eq!(drifted_files(root, &fp_path, 2, &current), None);

        fs::write(&fp_path, "{ truncated").unwrap();
        assert_eq!(load_fingerprint(&fp_path, 1), None, "corrupt file");

        // The tmp file from the atomic write never lingers.
        store_fingerprint(&fp_path, 1, &fp).unwrap();
        assert!(!fp_path.with_extension("json.tmp").exists());
    }
}
