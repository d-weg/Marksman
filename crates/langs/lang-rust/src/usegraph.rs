//! Freshness for the cached `rust-analyzer scip` graph — the rust twin of what lang-ts does
//! for its SCIP index. The cache is generated at index time (batch — a full `rust-analyzer
//! scip` is ~a cargo check, never on the live path), so without help it lies twice: silently
//! stale across sessions (no fingerprint), and blind to same-session committed edits. Fix, in
//! the same shape as lang-ts: a content fingerprint stored beside the cache decides how much
//! to trust it at load, and a per-file EDGE OVERLAY (tree-sitter `mod` + resolved `use` paths,
//! in-process, no rust-analyzer) re-describes exactly the drifted/edited files. Unchanged
//! files keep compiler-accurate scip edges; changed files get syntax-accurate edges instead
//! of stale or missing ones.
use ci_core::ImportGraph;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tree_sitter::Node as TsNode;

/// Bump when inputs/format change; old fingerprints then read as fully drifted.
const FP_VERSION: u64 = 1;

pub(crate) type Fingerprint = BTreeMap<String, String>;

pub(crate) fn fingerprint_path(root: &Path) -> PathBuf {
    root.join(".codeindex-rs").join("rust.scip.fingerprint.json")
}

/// FNV-1a 64-bit — detects accidental drift, not tampering (same rationale as lang-ts).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Hash every file that feeds `rust-analyzer scip`: the `.rs` sources plus the manifests
/// that shape the crate graph. Content hashes, not mtimes (a git checkout rewrites mtimes).
pub(crate) fn source_fingerprint(root: &Path) -> Fingerprint {
    let mut map = Fingerprint::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let is_input = name.ends_with(".rs") || name == "Cargo.toml" || name == "Cargo.lock";
        if !is_input {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.starts_with("target/") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(entry.path()) {
            map.insert(rel, format!("{:016x}", fnv1a(&bytes)));
        }
    }
    map
}

pub(crate) fn store_fingerprint(root: &Path) -> std::io::Result<()> {
    let payload = serde_json::json!({ "version": FP_VERSION, "files": source_fingerprint(root) });
    let path = fingerprint_path(root);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_vec(&payload)?)
}

/// Files that changed/appeared/disappeared since the fingerprint was stored — the set whose
/// scip edges can no longer be trusted. `None` = no usable fingerprint (treat EVERYTHING as
/// drifted: pre-fingerprint caches never get silently blessed).
pub(crate) fn drifted_files(root: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(fingerprint_path(root)).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if v.get("version").and_then(|x| x.as_u64()) != Some(FP_VERSION) {
        return None;
    }
    let stored: Fingerprint = serde_json::from_value(v.get("files")?.clone()).ok()?;
    let current = source_fingerprint(root);
    let mut drift: Vec<String> = Vec::new();
    for (k, h) in &current {
        if stored.get(k) != Some(h) {
            drift.push(k.clone());
        }
    }
    for k in stored.keys() {
        if !current.contains_key(k) {
            drift.push(k.clone());
        }
    }
    drift.sort();
    Some(drift)
}

/// Apply per-file overrides to a base graph: `Some(edges)` replaces the file's outgoing
/// edges (removing the entry when empty), `None` removes it (file deleted).
pub(crate) fn overlay_graph(
    mut base: ImportGraph,
    overrides: &std::collections::HashMap<String, Option<Vec<PathBuf>>>,
) -> ImportGraph {
    for (rel, edges) in overrides {
        let key = PathBuf::from(rel);
        match edges {
            Some(e) if !e.is_empty() => {
                base.insert(key, e.clone());
            }
            _ => {
                base.remove(&key);
            }
        }
    }
    base
}

// ── use-path resolution (tree-sitter, in-process) ────────────────────────────

/// Outgoing file edges of one rust file from its CURRENT content: `mod` declarations plus
/// `use crate::…`/`self::`/`super::` paths resolved to files. Conservative: an unresolvable
/// path (external crate, re-export chain, exotic layout) contributes no edge — a miss equals
/// today's mod-graph fidelity, never a wrong edge.
pub(crate) fn file_edges(root: &Path, from: &str, tree_root: TsNode, bytes: &[u8]) -> Vec<PathBuf> {
    let mut edges: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p != Path::new(from) && !edges.contains(&p) {
            edges.push(p);
        }
    };
    for module in super::mod_decls(tree_root, bytes) {
        if let Some(t) = super::resolve_mod(root, from, &module) {
            push(t);
        }
    }
    for path in use_paths_in(tree_root, bytes) {
        if let Some(t) = resolve_use(root, from, &path) {
            push(t);
        }
    }
    edges.sort();
    edges
}

/// All full `use` paths in the file, as segment lists (`use a::{b::C, d}` -> `[a,b,C]`,`[a,d]`).
fn use_paths_in(root: TsNode, bytes: &[u8]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "use_declaration" {
            if let Some(arg) = n.child_by_field_name("argument") {
                collect_paths(arg, bytes, &[], &mut out);
            }
            continue;
        }
        let mut cursor = n.walk();
        for c in n.named_children(&mut cursor) {
            stack.push(c);
        }
    }
    out
}

/// Expand one use-tree node into full segment paths, prefixed by `prefix`.
fn collect_paths(n: TsNode, bytes: &[u8], prefix: &[String], out: &mut Vec<Vec<String>>) {
    let text = |x: &TsNode| x.utf8_text(bytes).unwrap_or("").to_string();
    match n.kind() {
        "identifier" | "crate" | "super" | "self" | "metavariable" => {
            let mut p = prefix.to_vec();
            p.push(text(&n));
            out.push(p);
        }
        "scoped_identifier" | "scoped_use_list" | "use_wildcard" => {
            // path field (optional) extends the prefix; then the name/list/star completes it.
            let mut p = prefix.to_vec();
            if let Some(path) = n.child_by_field_name("path") {
                let mut nested = Vec::new();
                collect_paths(path, bytes, &[], &mut nested);
                if let Some(first) = nested.into_iter().next() {
                    p.extend(first);
                }
            }
            match n.kind() {
                "scoped_identifier" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        p.push(text(&name));
                    }
                    out.push(p);
                }
                "scoped_use_list" => {
                    if let Some(list) = n.child_by_field_name("list") {
                        collect_paths(list, bytes, &p, out);
                    }
                }
                _ => out.push(p), // use_wildcard: the prefix path itself is the edge candidate
            }
        }
        "use_list" => {
            let mut cursor = n.walk();
            for c in n.named_children(&mut cursor) {
                collect_paths(c, bytes, prefix, out);
            }
        }
        "use_as_clause" => {
            if let Some(path) = n.child_by_field_name("path") {
                collect_paths(path, bytes, prefix, out);
            }
        }
        _ => {}
    }
}

/// The `src/`-style module base of `from`'s own scope: `mod.rs`/`lib.rs`/`main.rs` own their
/// directory; `foo.rs` owns `foo/` (same rule as `resolve_mod`).
fn own_scope_dir(from: &str) -> PathBuf {
    let p = Path::new(from);
    let parent = p.parent().unwrap_or(Path::new("")).to_path_buf();
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "mod" | "lib" | "main") {
        parent
    } else {
        parent.join(stem)
    }
}

/// Resolve one absolute-ish use path to the repo-relative file defining its deepest file-module
/// prefix. `crate::a::b::Item` -> `src/a/b.rs` (or as deep as file modules go). External crate
/// paths (bare first segment) and std/core/alloc resolve to `None`.
fn resolve_use(root: &Path, from: &str, segs: &[String]) -> Option<PathBuf> {
    let mut i = 0;
    let (mut dir, mut last_file): (PathBuf, Option<PathBuf>) = match segs.first()?.as_str() {
        "crate" => {
            i = 1;
            let (src, entry) = crate_src(root, from)?;
            (src, Some(entry))
        }
        "self" => {
            i = 1;
            (own_scope_dir(from), Some(PathBuf::from(from)))
        }
        "super" => {
            let mut dir = own_scope_dir(from);
            while segs.get(i).map(String::as_str) == Some("super") {
                i += 1;
                dir = dir.parent()?.to_path_buf();
            }
            // The parent scope's own file, for a `use super::item` with no deeper module.
            let file = scope_file(root, &dir);
            (dir, file)
        }
        _ => return None, // bare first segment: an external crate (edition 2018+)
    };
    while let Some(seg) = segs.get(i) {
        let as_file = dir.join(format!("{seg}.rs"));
        let as_dir = dir.join(seg).join("mod.rs");
        if root.join(&as_file).is_file() {
            last_file = Some(as_file);
            dir = dir.join(seg);
        } else if root.join(&as_dir).is_file() {
            last_file = Some(as_dir);
            dir = dir.join(seg);
        } else {
            break; // `seg` is an item (or an unresolvable module): stop at the deepest file
        }
        i += 1;
    }
    last_file.filter(|f| f != Path::new(from))
}

/// The file that IS the module scope of `dir`: its `mod.rs`, its sibling `<dir>.rs` (file
/// module), or the crate entry when `dir` is a `src/` root.
fn scope_file(root: &Path, dir: &Path) -> Option<PathBuf> {
    let mod_rs = dir.join("mod.rs");
    if root.join(&mod_rs).is_file() {
        return Some(mod_rs);
    }
    if let (Some(parent), Some(name)) = (dir.parent(), dir.file_name().and_then(|s| s.to_str())) {
        let file_mod = parent.join(format!("{name}.rs"));
        if root.join(&file_mod).is_file() {
            return Some(file_mod);
        }
    }
    for entry in ["lib.rs", "main.rs"] {
        let f = dir.join(entry);
        if root.join(&f).is_file() {
            return Some(f);
        }
    }
    None
}

/// `from`'s crate: the `src/` dir and entry file (`lib.rs`, else `main.rs`) of the nearest
/// enclosing package (walking up to the first dir with a `Cargo.toml`).
fn crate_src(root: &Path, from: &str) -> Option<(PathBuf, PathBuf)> {
    let mut dir = Path::new(from).parent().unwrap_or(Path::new("")).to_path_buf();
    loop {
        if root.join(&dir).join("Cargo.toml").is_file() {
            let src = dir.join("src");
            for entry in ["lib.rs", "main.rs"] {
                let f = src.join(entry);
                if root.join(&f).is_file() {
                    return Some((src, f));
                }
            }
            return None;
        }
        if dir.as_os_str().is_empty() {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parse(content: &str) -> tree_sitter::Tree {
        crate::RustProvider::parse(content).unwrap()
    }

    #[test]
    fn resolves_crate_self_super_use_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname=\"t\"\n").unwrap();
        fs::create_dir_all(root.join("src/parse")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod lexer;\nmod parse;\n").unwrap();
        fs::write(root.join("src/lexer.rs"), "pub struct Token;\n").unwrap();
        fs::write(root.join("src/parse/mod.rs"), "pub mod expr;\npub fn helper() {}\n").unwrap();
        fs::write(root.join("src/parse/expr.rs"), "").unwrap();

        let content = "use crate::lexer::Token;\nuse crate::parse::{expr, helper};\nuse serde::Serialize;\n";
        fs::write(root.join("src/parse/expr.rs"), content).unwrap();
        let tree = parse(content);
        let edges = file_edges(root, "src/parse/expr.rs", tree.root_node(), content.as_bytes());
        assert!(edges.contains(&PathBuf::from("src/lexer.rs")), "crate:: use edge: {edges:?}");
        assert!(edges.contains(&PathBuf::from("src/parse/mod.rs")), "helper resolves to parent mod.rs: {edges:?}");
        assert!(!edges.iter().any(|e| e.to_string_lossy().contains("serde")), "external crate skipped: {edges:?}");

        // super:: from a mod.rs child file
        let content2 = "use super::Token;\n";
        let tree2 = parse(content2);
        fs::write(root.join("src/parse/expr.rs"), content2).unwrap();
        let edges2 = file_edges(root, "src/parse/expr.rs", tree2.root_node(), content2.as_bytes());
        assert_eq!(edges2, vec![PathBuf::from("src/parse/mod.rs")], "super -> parent scope file: {edges2:?}");
    }

    #[test]
    fn fingerprint_reports_drift_and_overlay_applies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname=\"t\"\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod a;\n").unwrap();
        fs::write(root.join("src/a.rs"), "pub fn f() {}\n").unwrap();

        store_fingerprint(root).unwrap();
        assert_eq!(drifted_files(root), Some(vec![]), "fresh fingerprint: no drift");

        fs::write(root.join("src/a.rs"), "pub fn f() {}\npub fn g() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "").unwrap();
        let drift = drifted_files(root).unwrap();
        assert_eq!(drift, vec!["src/a.rs".to_string(), "src/b.rs".to_string()], "changed + new: {drift:?}");

        // No fingerprint at all -> None (everything untrusted).
        fs::remove_file(fingerprint_path(root)).unwrap();
        assert_eq!(drifted_files(root), None);

        // Overlay semantics: replace, blank, delete.
        let mut base = ImportGraph::new();
        base.insert(PathBuf::from("src/a.rs"), vec![PathBuf::from("src/lib.rs")]);
        base.insert(PathBuf::from("src/gone.rs"), vec![PathBuf::from("src/a.rs")]);
        let mut ov = std::collections::HashMap::new();
        ov.insert("src/a.rs".to_string(), Some(vec![PathBuf::from("src/b.rs")]));
        ov.insert("src/gone.rs".to_string(), None);
        let g = overlay_graph(base, &ov);
        assert_eq!(g.get(&PathBuf::from("src/a.rs")).unwrap(), &vec![PathBuf::from("src/b.rs")]);
        assert!(!g.contains_key(&PathBuf::from("src/gone.rs")));
    }
}
