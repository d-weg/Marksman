//! moves — the generic move-rewrite engine and deleted-reference diagnostics
//! (provider-contract §8, extracted with lang-rust's implementations as the reference
//! semantics).
//!
//! A file move has three universal concerns: (a) how code REFERENCES a file (Rust `crate::`
//! paths, TS relative specifiers, Java FQNs, Python dotted modules), (b) how a file is
//! DECLARED a project member (`mod x;`, `__init__.py`, barrels, a `package` line), and (c)
//! rewriting (a) and maintaining (b) as ONE WorkspaceEdit. The mechanics — file walking,
//! span edits, CreateFile ops, WorkspaceEdit assembly — are language-independent and live
//! HERE; a provider supplies only the three syntax hooks ([`MoveModel`]). The same
//! `ref_occurrences` hook powers [`deleted_reference_diags`]: any surviving file's reference
//! resolving to a batch-deleted path is a diagnostic — the unresolved-import class an
//! engine's own diagnostics may miss.
//!
//! Best-effort BY DESIGN, exactly like the reference implementation: a model declines the
//! shapes it doesn't understand (`None` from a hook), the caller falls through to its
//! engine-native path, and the type-check gate rejects any rewrite that comes out wrong —
//! the fallback can be incomplete, it can never be silently wrong.
use ci_core::Diag;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub mod dotted;

/// One reference to a project file, as found by [`MoveModel::ref_occurrences`].
#[derive(Debug, Clone)]
pub struct RefOccurrence {
    /// 0-based line index in the scanned content.
    pub line: usize,
    /// Byte columns (within the line) of the reference path text — the exact span a move
    /// replaces with the new file's reference. `None` when the occurrence isn't
    /// span-rewritable (a membership declaration, a non-contiguous path): deletion
    /// diagnostics still see it; the move rewriter leaves it to `membership_edits`.
    pub span: Option<(usize, usize)>,
    /// The repo-relative path this reference resolves to. A CANDIDATE, not a disk fact:
    /// deletion diagnostics match it against batch-deleted paths, which are already off
    /// disk when the gate runs.
    pub target: String,
    /// The language's diagnostic message for this reference when `target` is deleted by
    /// the batch. `None` = the occurrence feeds move rewrites only (e.g. an in-scope bare
    /// path the language's deletion story doesn't flag).
    pub note: Option<String>,
}

/// One declaration-maintenance edit from [`MoveModel::membership_edits`] — the vocabulary
/// the engine can assemble into a WorkspaceEdit. Lines are 0-based; `ReplaceLine`/
/// `DeleteLine` lines are excluded from reference rewriting (the declaration is maintained
/// here, not by the span pass).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipEdit {
    /// Rewrite the whole `line` of an existing file.
    ReplaceLine { file: String, line: usize, new_text: String },
    /// Delete the whole `line` (its newline included).
    DeleteLine { file: String, line: usize },
    /// Insert `text` (carrying its own trailing newline) at the start of `line`;
    /// `line == the file's line count` appends at EOF.
    InsertAt { file: String, line: usize, text: String },
    /// Create `file` with `content` (a CreateFile resource op + its content edit).
    CreateFile { file: String, content: String },
}

/// The three syntax hooks of contract §8. Everything else about a move — which files to
/// scan, how to splice a span, how a WorkspaceEdit is shaped — is the engine's.
pub trait MoveModel {
    /// How code references `rel` (Rust `text::tokenize`, Java `a.b.C`). `None` = the file
    /// isn't reference-addressable in this language (the engine declines the move).
    fn file_to_ref(&self, rel: &str) -> Option<String>;
    /// Every file-reference in `content`. `rel` names the scanned file so scope-sensitive
    /// references (a parent's bare child paths) resolve correctly.
    fn ref_occurrences(&self, rel: &str, content: &str) -> Vec<RefOccurrence>;
    /// Declaration maintenance for moving `from` → `to`. `None` = the move shape is
    /// outside the model (the engine declines); an empty vec = the language needs no
    /// membership work (files are members by existing).
    fn membership_edits(&self, from: &str, to: &str) -> Option<Vec<MembershipEdit>>;
    /// Whether the repo-relative `rel` is a source file that can carry references — the
    /// engine's walk filter.
    fn is_source(&self, rel: &str) -> bool;
}

/// One whole-line TextEdit (LSP 0-based; the line INCLUDING its newline when deleting).
fn line_edit(line_idx: usize, old_line: &str, new_text: &str, delete_line: bool) -> Value {
    let (end_line, end_char) = if delete_line && new_text.is_empty() {
        (line_idx + 1, 0) // swallow the newline
    } else {
        (line_idx, old_line.len()) // byte columns — the core Range contract
    };
    json!({
        "range": {"start": {"line": line_idx, "character": 0}, "end": {"line": end_line, "character": end_char}},
        "newText": new_text,
    })
}

/// `line` with `new_text` spliced over each span — right-to-left, so earlier spans keep
/// their byte positions. Spans must not overlap; one that falls outside the line (or off a
/// char boundary) is skipped, never a panic — a bad hook span degrades to "not rewritten",
/// and the gate judges the result.
pub fn splice_spans(line: &str, spans: &[(usize, usize)], new_text: &str) -> String {
    let mut sorted = spans.to_vec();
    sorted.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = line.to_string();
    for (s, e) in sorted {
        if line.get(s..e).is_none() {
            continue;
        }
        out.replace_range(s..e, new_text);
    }
    out
}

/// Repo-relative source files under `root` (gitignore-aware walk), filtered by the model —
/// the file-walking half the engine owns.
fn source_files<M: MoveModel + ?Sized>(root: &Path, model: &M) -> Vec<String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if let Ok(rel) = entry.path().strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !rel.is_empty() && model.is_source(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

/// The move's WorkspaceEdit (LSP `documentChanges`: CreateFile + TextEdits) for moving
/// `from` → `to`, or `None` when the model declines the shape (either path not
/// reference-addressable, identical references, membership outside the model, or an
/// unreadable source file). Rewrites are whole-line edits: every occurrence targeting
/// `from` has the new reference spliced over its span; lines a membership edit rewrites or
/// deletes are excluded (the declaration is maintained by that edit, not re-rewritten).
pub fn move_workspace_edit<M: MoveModel + ?Sized>(
    root: &Path,
    from: &str,
    to: &str,
    model: &M,
) -> Option<Value> {
    let old_ref = model.file_to_ref(from)?;
    let new_ref = model.file_to_ref(to)?;
    if old_ref == new_ref {
        return None; // nothing references differently — not a move this engine can improve
    }
    let membership = model.membership_edits(from, to)?;

    let uri = |rel: &str| format!("file://{}/{}", root.to_string_lossy(), rel);
    let mut doc_changes: Vec<Value> = Vec::new();

    // Membership: creations become resource ops up front (a created file's content edit
    // follows its create op — the ordering `apply_workspace_edit` relies on); line edits
    // group per file and ride ahead of that file's reference rewrites.
    let mut per_file: HashMap<&str, Vec<&MembershipEdit>> = HashMap::new();
    let mut file_order: Vec<&str> = Vec::new();
    for m in &membership {
        match m {
            MembershipEdit::CreateFile { file, content } => {
                doc_changes.push(json!({"kind": "create", "uri": uri(file)}));
                doc_changes.push(json!({
                    "textDocument": {"uri": uri(file), "version": null},
                    "edits": [ {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                                "newText": content} ],
                }));
            }
            MembershipEdit::ReplaceLine { file, .. }
            | MembershipEdit::DeleteLine { file, .. }
            | MembershipEdit::InsertAt { file, .. } => {
                if !per_file.contains_key(file.as_str()) {
                    file_order.push(file);
                }
                per_file.entry(file).or_default().push(m);
            }
        }
    }

    // Render one file's membership edits against its current content; `skip` collects the
    // lines whose whole text the membership pass owns.
    let render = |content: &str, edits: &[&MembershipEdit], skip: &mut HashSet<usize>| -> Vec<Value> {
        let lines: Vec<&str> = content.lines().collect();
        let mut out = Vec::new();
        for m in edits {
            match m {
                MembershipEdit::ReplaceLine { line, new_text, .. } => {
                    skip.insert(*line);
                    out.push(line_edit(*line, lines.get(*line).copied().unwrap_or(""), new_text, false));
                }
                MembershipEdit::DeleteLine { line, .. } => {
                    skip.insert(*line);
                    out.push(line_edit(*line, lines.get(*line).copied().unwrap_or(""), "", true));
                }
                MembershipEdit::InsertAt { line, text, .. } => {
                    out.push(json!({
                        "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 0}},
                        "newText": text,
                    }));
                }
                MembershipEdit::CreateFile { .. } => unreachable!("creations are resource ops, handled above"),
            }
        }
        out
    };

    let mut visited: HashSet<String> = HashSet::new();
    for rel in source_files(root, model) {
        if rel == from {
            // The moved file's REFERENCES travel as-is (it points at the same files after the
            // move), but its own MEMBERSHIP declaration may need rewriting — a package/namespace
            // line lives inside the file itself (Java `package p;`), unlike Rust's `mod x;` which
            // lives in the parent. A membership edit targeting `from` is rendered against `from`'s
            // content here; the edit rides the `from` URI, so `apply_move` splices it BEFORE the
            // move carries the (now-corrected) content to `to`.
            if let Some(ms) = per_file.get(rel.as_str()) {
                if let Ok(content) = std::fs::read_to_string(root.join(&rel)) {
                    let mut skip = HashSet::new();
                    let edits = render(&content, ms, &mut skip);
                    if !edits.is_empty() {
                        doc_changes.push(json!({"textDocument": {"uri": uri(&rel), "version": null}, "edits": edits}));
                    }
                    visited.insert(rel.clone());
                }
            }
            continue;
        }
        let content = std::fs::read_to_string(root.join(&rel)).ok()?;
        visited.insert(rel.clone());
        let mut skip: HashSet<usize> = HashSet::new();
        let mut edits: Vec<Value> =
            per_file.get(rel.as_str()).map(|ms| render(&content, ms, &mut skip)).unwrap_or_default();
        let mut spans_by_line: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
        for occ in model.ref_occurrences(&rel, &content) {
            if occ.target == from {
                if let Some(span) = occ.span {
                    spans_by_line.entry(occ.line).or_default().push(span);
                }
            }
        }
        for (i, line) in content.lines().enumerate() {
            if skip.contains(&i) {
                continue; // this line's whole text is a membership edit's
            }
            let Some(spans) = spans_by_line.get(&i) else { continue };
            let rewritten = splice_spans(line, spans, &new_ref);
            if rewritten != line {
                edits.push(line_edit(i, line, &rewritten, false));
            }
        }
        if !edits.is_empty() {
            doc_changes.push(json!({"textDocument": {"uri": uri(&rel), "version": null}, "edits": edits}));
        }
    }
    // Membership files outside the walk (a manifest that isn't a source file).
    for rel in file_order {
        if visited.contains(rel) || rel == from {
            continue;
        }
        let content = std::fs::read_to_string(root.join(rel)).ok()?;
        let mut skip = HashSet::new();
        let edits = render(&content, &per_file[rel], &mut skip);
        if !edits.is_empty() {
            doc_changes.push(json!({"textDocument": {"uri": uri(rel), "version": null}, "edits": edits}));
        }
    }
    Some(json!({"documentChanges": doc_changes}))
}

/// Diagnostics for references to files the CURRENT BATCH deletes (empty-content buffers,
/// the gate's deletion convention): each surviving buffer's references resolve through the
/// model, and any noted occurrence whose target is a deleted path is a diagnostic. This is
/// the unresolved-import class a language engine's own diagnostics may never report — the
/// generic form of lang-rust's `deleted_path_references` gap-fill.
pub fn deleted_reference_diags<M: MoveModel + ?Sized>(model: &M, files: &[(String, String)]) -> Vec<Diag> {
    let deleted: HashSet<&str> =
        files.iter().filter(|(_, c)| c.is_empty()).map(|(f, _)| f.as_str()).collect();
    if deleted.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (rel, content) in files.iter().filter(|(_, c)| !c.is_empty()) {
        for occ in model.ref_occurrences(rel, content) {
            let Some(note) = occ.note else { continue };
            if deleted.contains(occ.target.as_str()) {
                out.push(Diag { file: rel.clone(), code: 0, message: note, line: occ.line as u32 + 1 });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_vfs::Vfs;
    use std::fs;

    // A toy language for the engine mechanics: `.x` files; `load <name>;` references
    // `<name>.x` (noted for deletion diagnostics); `use <name>;` references it silently
    // (rewrite-only, note = None). Membership: `manifest.txt` (NOT a source file — it
    // exercises the leftover pass) lists members one per line.
    struct ToyModel;
    impl MoveModel for ToyModel {
        fn file_to_ref(&self, rel: &str) -> Option<String> {
            rel.strip_suffix(".x").map(str::to_string)
        }
        fn ref_occurrences(&self, _rel: &str, content: &str) -> Vec<RefOccurrence> {
            let mut out = Vec::new();
            for (i, l) in content.lines().enumerate() {
                for (kw, noted) in [("load ", true), ("use ", false)] {
                    let Some(at) = l.find(kw) else { continue };
                    let start = at + kw.len();
                    let Some(end) = l[start..].find(';').map(|e| start + e) else { continue };
                    let name = &l[start..end];
                    out.push(RefOccurrence {
                        line: i,
                        span: Some((start, end)),
                        target: format!("{name}.x"),
                        note: noted.then(|| format!("`{name}` is deleted by this batch")),
                    });
                }
            }
            out
        }
        fn membership_edits(&self, from: &str, to: &str) -> Option<Vec<MembershipEdit>> {
            let old = self.file_to_ref(from)?;
            let new = self.file_to_ref(to)?;
            Some(vec![
                MembershipEdit::ReplaceLine { file: "manifest.txt".into(), line: 0, new_text: format!("member {new}") },
                MembershipEdit::CreateFile { file: "sub/manifest.txt".into(), content: format!("member {new}\n") },
                MembershipEdit::InsertAt { file: "manifest.txt".into(), line: 2, text: format!("moved-from {old}\n") },
                // A membership-owned line in a SOURCE file that also matches the ref
                // scanner: the span pass must skip it (one owner per line).
                MembershipEdit::ReplaceLine { file: "decl.x".into(), line: 0, new_text: format!("decl {new};") },
            ])
        }
        fn is_source(&self, rel: &str) -> bool {
            rel.ends_with(".x")
        }
    }

    fn toy_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.x"), "body of a\n").unwrap();
        fs::write(root.join("b.x"), "load a;\nuse a;\nload other;\n").unwrap();
        fs::write(root.join("decl.x"), "load a; -- membership-owned line\nload a;\n").unwrap();
        fs::write(root.join("manifest.txt"), "member a\nmember b\n").unwrap();
        dir
    }

    // The engine's whole assembly, applied for real: reference spans spliced (noted and
    // silent alike), the moved file itself untouched, membership vocabulary (replace /
    // delete-skip / insert / create) rendered, and a membership-owned line never
    // double-edited by the span pass.
    #[test]
    fn move_assembles_rewrites_membership_and_creates() {
        let dir = toy_repo();
        let root = dir.path();
        let we = move_workspace_edit(root, "a.x", "d/a.x", &ToyModel).expect("toy move");

        let mut vfs = Vfs::new(root);
        crate::apply::apply_workspace_edit(&mut vfs, root, &we).expect("edit applies");

        let b = vfs.read(Path::new("b.x")).expect("b.x rewritten");
        assert_eq!(b, "load d/a;\nuse d/a;\nload other;\n", "both occurrence kinds spliced, others untouched");
        assert!(!vfs.is_staged(Path::new("a.x")), "the moved file's own content is never rewritten");

        let decl = vfs.read(Path::new("decl.x")).expect("decl.x edited");
        assert_eq!(
            decl, "decl d/a;\nload d/a;\n",
            "membership owns line 0 (no double edit); the span pass still rewrites line 1"
        );

        let manifest = vfs.read(Path::new("manifest.txt")).expect("leftover pass reaches non-source files");
        assert_eq!(manifest, "member d/a\nmember b\nmoved-from a\n", "replace + EOF insert rendered");

        let created = vfs.read(Path::new("sub/manifest.txt")).expect("CreateFile materialized in the overlay");
        assert_eq!(created, "member d/a\n");
    }

    #[test]
    fn move_declines_when_a_hook_declines() {
        let dir = toy_repo();
        let root = dir.path();
        // Not reference-addressable (either end).
        assert!(move_workspace_edit(root, "a.y", "d/a.x", &ToyModel).is_none());
        assert!(move_workspace_edit(root, "a.x", "d/a.y", &ToyModel).is_none());
        // Identical reference: nothing to rewrite.
        assert!(move_workspace_edit(root, "a.x", "a.x", &ToyModel).is_none());
        // Membership outside the model → the engine declines (caller falls through).
        struct NoMembership;
        impl MoveModel for NoMembership {
            fn file_to_ref(&self, rel: &str) -> Option<String> {
                ToyModel.file_to_ref(rel)
            }
            fn ref_occurrences(&self, rel: &str, content: &str) -> Vec<RefOccurrence> {
                ToyModel.ref_occurrences(rel, content)
            }
            fn membership_edits(&self, _from: &str, _to: &str) -> Option<Vec<MembershipEdit>> {
                None
            }
            fn is_source(&self, rel: &str) -> bool {
                ToyModel.is_source(rel)
            }
        }
        assert!(move_workspace_edit(root, "a.x", "d/a.x", &NoMembership).is_none());
    }

    #[test]
    fn deleted_reference_diags_flag_noted_refs_to_deleted_paths_only() {
        let files = vec![
            ("b.x".to_string(), "load a;\nuse a;\nload other;\n".to_string()),
            ("a.x".to_string(), String::new()), // the deletion stand-in
        ];
        let diags = deleted_reference_diags(&ToyModel, &files);
        // `use a;` (note = None) and `load other;` (target survives) stay silent.
        assert_eq!(diags.len(), 1, "exactly the noted reference to the deleted path: {diags:?}");
        assert_eq!((diags[0].file.as_str(), diags[0].line), ("b.x", 1));
        assert_eq!(diags[0].message, "`a` is deleted by this batch");

        // No deletions in the batch → the fast path returns nothing.
        let files = vec![("b.x".to_string(), "load a;\n".to_string())];
        assert!(deleted_reference_diags(&ToyModel, &files).is_empty());
    }

    #[test]
    fn splice_spans_is_right_to_left_and_bound_safe() {
        assert_eq!(splice_spans("load a; use a;", &[(5, 6), (12, 13)], "d/a"), "load d/a; use d/a;");
        // An out-of-range span is skipped, not a panic.
        assert_eq!(splice_spans("load a;", &[(5, 6), (40, 44)], "d/a"), "load d/a;");
    }
}
