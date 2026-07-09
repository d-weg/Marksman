//! movefix — the Java §8 hooks ([`ci_edit::moves::MoveModel`]) behind the shared move engine.
//!
//! jdtls's `willRenameFiles` IS engine-native for Java moves (it rewrites the package decl AND
//! importers) and is preferred WHERE AVAILABLE — the exact ordering lang-rust settled for
//! rust-analyzer (engine-native first, the syntactic fallback second, the gate over both). But
//! jdtls is push-diagnostics-only, minutes-cold, and often absent; these hooks are the runnable
//! fallback that gives Java complete one-call moves without it. What is Java about a move lives
//! HERE; the file walking / span splicing / WorkspaceEdit assembly is `ci_edit::moves`.
//!
//! The three concerns, Java edition:
//! - **`file_to_ref`**: path → fully-qualified name (`src/main/java/com/x/A.java` → `com.x.A`),
//!   inverting the source-root resolution the import graph already uses (one resolver, via
//!   `lang_fallback::java`).
//! - **`ref_occurrences`**: `import a.b.C;` declarations (span-rewritable, noted for deletion
//!   diagnostics) plus fully-qualified `a.b.C` mentions in code (rewrite-only — the compiler
//!   gate catches a stranded FQN the imports don't cover). Each resolves through the shared
//!   Java import resolver so a reference to a batch-deleted class is a diagnostic.
//! - **`membership_edits`**: rewrite the `package` declaration line to match the new directory.
//!   Java has NO per-file membership file like Rust's `mod.rs`/parent `mod x;` — a class is a
//!   member of its package by living in the matching directory — so this hook is
//!   package-line-only by design, never a create/delete of a declaration file.
//!
//! Best-effort BY DESIGN, exactly like the reference (`lang-rust::movefix`): a shape the model
//! doesn't understand returns `None`, the caller falls through to jdtls, and the javac gate
//! rejects any rewrite that comes out wrong — the fallback can be incomplete, never silently
//! wrong.
use ci_edit::moves::{MembershipEdit, MoveModel, RefOccurrence};
use lang_fallback::java;
use std::path::Path;

/// The Java [`MoveModel`]: FQN references and package-declaration membership.
pub(crate) struct JavaMoveModel<'a>(pub(crate) &'a Path);

/// Byte spans of the exact FQN token `fqn` in `line`, bounded so `com.x.A` never matches inside
/// `com.x.Abc` or `xcom.x.A`. The boundary rule is ASYMMETRIC: a LEADING identifier char or `.`
/// means we sit inside a longer, more-qualified name (`zcom.x.A`, `a.com.x.A`) — not this class.
/// A TRAILING identifier char means a longer simple name (`com.x.Abc`), but a trailing `.` is
/// member access on the class (`com.x.A.f()`) or a nested type (`com.x.A.B`) — both DO reference
/// `com.x.A`, so a trailing `.` is a valid boundary.
fn fqn_spans(line: &str, fqn: &str) -> Vec<(usize, usize)> {
    let before_ext = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
    let after_ext = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = line[from..].find(fqn) {
        let at = from + pos;
        let end = at + fqn.len();
        let before_ok = at == 0 || !line[..at].chars().next_back().is_some_and(before_ext);
        let after_ok = line[end..].chars().next().is_none_or(|c| !after_ext(c));
        if before_ok && after_ok {
            out.push((at, end));
        }
        from = end;
    }
    out
}

impl MoveModel for JavaMoveModel<'_> {
    fn file_to_ref(&self, rel: &str) -> Option<String> {
        java::file_to_fqn(self.0, rel)
    }

    /// Two occurrence kinds per line:
    /// - `import a.b.C;` declarations: the FQN is a span-rewritable reference resolving to a
    ///   file (noted for the deletion pass — an import of a deleted class is the unresolved
    ///   symbol javac's own diagnostics report, but the anchored reject-recipe wants it too).
    /// - fully-qualified `a.b.C` mentions elsewhere (`new com.x.A()`, `com.x.A.field`): the
    ///   longest dotted prefix resolving to a source file is a rewrite span (`note: None` — the
    ///   compiler catches a stranded expression FQN; this just keeps a move complete).
    fn ref_occurrences(&self, rel: &str, content: &str) -> Vec<RefOccurrence> {
        let mut out = Vec::new();
        // A fully-qualified name inside a STRING or COMMENT is not a reference to rewrite: a
        // rewrite there still compiles, so the type-check gate can't catch it. Mask those extents
        // via tree-sitter (exact for block comments, text blocks, escaped strings) and skip any
        // code-mention run that starts inside one (M1). Imports never sit in strings, so the
        // import branch below stays unmasked.
        let masked = lang_fallback::string_comment_spans(lang_fallback::FbLang::Java, content);
        let line_starts = lang_fallback::line_start_offsets(content);
        let in_string_or_comment = |abs: usize| masked.iter().any(|&(s, e)| abs >= s && abs < e);
        for (i, line) in content.lines().enumerate() {
            let t = line.trim_start();
            let import_fqn = t
                .strip_prefix("import ")
                .map(|r| r.trim_start().strip_prefix("static ").map(str::trim_start).unwrap_or(r.trim_start()))
                .and_then(|r| r.split(';').next())
                .map(str::trim);
            if let Some(fqn) = import_fqn {
                // On-demand `import a.b.*;` resolves to the package dir, not one file — the move
                // rewriter leaves those to the moved file's package staying importable by
                // directory; only single-type imports name a file to retarget.
                if !fqn.ends_with('*') && !fqn.is_empty() {
                    if let Some(target) = java::resolve_import(self.0, rel, fqn) {
                        let target = target.to_string_lossy().replace('\\', "/");
                        let note = format!(
                            "unresolved import `{fqn}` — {target} is deleted/moved by this batch; update the import"
                        );
                        for span in fqn_spans(line, fqn) {
                            out.push(RefOccurrence { line: i, span: Some(span), target: target.clone(), note: Some(note.clone()) });
                        }
                    }
                }
                continue; // an import line carries no other FQN references worth scanning
            }
            // Fully-qualified references in code: walk dotted identifiers, resolve the longest
            // prefix that lands on a source file. Rewrite-only (the gate owns correctness).
            for (start, dotted) in dotted_runs(line) {
                if in_string_or_comment(line_starts[i] + start) {
                    continue; // a name inside a string/comment is never a rewrite target (M1)
                }
                let segs: Vec<&str> = dotted.split('.').collect();
                for n in (1..=segs.len()).rev() {
                    let prefix = segs[..n].join(".");
                    if let Some(target) = java::resolve_import(self.0, rel, &prefix) {
                        let target = target.to_string_lossy().replace('\\', "/");
                        let span = (start, start + prefix.len());
                        out.push(RefOccurrence { line: i, span: Some(span), target, note: None });
                        break; // longest resolving prefix wins; don't also emit its parents
                    }
                }
            }
        }
        out
    }

    /// Package-line-only membership (Java has no `mod.rs`): moving `from`→`to` across packages
    /// rewrites the moved file's own `package p;` line to the destination package. Same-package
    /// moves (a pure rename, same directory) need no package edit — an empty vec, not `None`, so
    /// the engine still rewrites importers. `None` only when either path isn't a resolvable
    /// `.java` FQN (the engine declines and the caller falls through to jdtls).
    fn membership_edits(&self, from: &str, to: &str) -> Option<Vec<MembershipEdit>> {
        let from_fqn = java::file_to_fqn(self.0, from)?;
        let to_fqn = java::file_to_fqn(self.0, to)?;
        let from_pkg = fqn_package(&from_fqn);
        let to_pkg = fqn_package(&to_fqn);
        if from_pkg == to_pkg {
            return Some(Vec::new()); // same package: importers rewrite, the package line stays
        }
        // The moved file's own `package` declaration must name the destination package. The
        // engine renders membership edits against the file's CURRENT content and skips those
        // lines from the reference pass — but the moved file travels as-is (the engine never
        // scans `from`), so the edit rides on the moved file, which the engine renders against
        // `from`'s content BEFORE the file moves. One shared scanner (`java::package_decl`), §7.
        let content = std::fs::read_to_string(self.0.join(from)).ok()?;
        match java::package_decl(&content) {
            Some((idx, _)) => {
                // Moved into the default package → drop the declaration (empty replacement line).
                let new_line = if to_pkg.is_empty() { String::new() } else { format!("package {to_pkg};") };
                Some(vec![MembershipEdit::ReplaceLine { file: from.to_string(), line: idx, new_text: new_line }])
            }
            None if to_pkg.is_empty() => Some(Vec::new()), // default → default: nothing to declare
            None => {
                // No package today (default package), moving INTO one: ADD the declaration before
                // the first real statement. A missing InsertAt used to decline the whole move (M3).
                let at = java_insert_line(&content);
                Some(vec![MembershipEdit::InsertAt {
                    file: from.to_string(),
                    line: at,
                    text: format!("package {to_pkg};\n"),
                }])
            }
        }
    }

    fn is_source(&self, rel: &str) -> bool {
        rel.ends_with(".java")
    }
}

/// The package part of an FQN (`com.x.A` → `com.x`; a top-level `A` → `""`).
fn fqn_package(fqn: &str) -> String {
    match fqn.rfind('.') {
        Some(i) => fqn[..i].to_string(),
        None => String::new(),
    }
}

/// The line to insert a `package` declaration at: before the first real statement (skipping a
/// leading license comment / blank lines), else line 0. (Finding the EXISTING declaration is
/// `java::package_decl` — one shared scanner, §7.)
fn java_insert_line(content: &str) -> usize {
    for (i, line) in content.lines().enumerate() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        return i;
    }
    0
}

/// Maximal `[A-Za-z0-9_.]` runs in `line` that contain a `.` and start on an identifier char —
/// the candidate dotted references (`com.x.A`, `a.b.c()` gives `a.b.c`). Each is `(start byte,
/// text)`; a leading/trailing dot is trimmed off the run.
fn dotted_runs(line: &str) -> Vec<(usize, String)> {
    let ext = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < line.len() && line[i..].chars().next().is_some_and(ext) {
                i += line[i..].chars().next().unwrap().len_utf8();
            }
            let run = line[start..i].trim_end_matches('.');
            if run.contains('.') {
                out.push((start, run.to_string()));
            }
        } else {
            i += c.len_utf8();
        }
    }
    out
}

/// The move's `WorkspaceEdit` over [`JavaMoveModel`], or `None` when the move shape is outside
/// what this fallback understands (jdtls, then the javac gate, still protect the commit).
pub(crate) fn move_workspace_edit(root: &Path, from: &str, to: &str) -> Option<serde_json::Value> {
    ci_edit::moves::move_workspace_edit(root, from, to, &JavaMoveModel(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_edit::moves::splice_spans;

    #[test]
    fn fqn_spans_are_token_bounded() {
        assert_eq!(fqn_spans("import com.x.A;", "com.x.A"), vec![(7, 14)]);
        // Never inside a longer FQN.
        assert_eq!(fqn_spans("import com.x.Abc;", "com.x.A"), vec![]);
        assert_eq!(fqn_spans("x = com.x.A.f() + zcom.x.A;", "com.x.A"), vec![(4, 11)]);
    }

    #[test]
    fn file_to_ref_inverts_source_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/main/java/com/x")).unwrap();
        std::fs::write(root.join("src/main/java/com/x/A.java"), "package com.x;\npublic class A {}\n").unwrap();
        let m = JavaMoveModel(root);
        assert_eq!(m.file_to_ref("src/main/java/com/x/A.java").as_deref(), Some("com.x.A"));
        assert_eq!(m.file_to_ref("README.md"), None);
    }

    // The move assembly: an importer's `import` line retargets to the new FQN, and the moved
    // file's own `package` declaration is rewritten to the destination package.
    #[test]
    fn cross_package_move_rewrites_import_and_package_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/main/java/com/x")).unwrap();
        std::fs::create_dir_all(root.join("src/main/java/com/y")).unwrap();
        std::fs::write(root.join("src/main/java/com/x/Helper.java"), "package com.x;\npublic class Helper {}\n").unwrap();
        std::fs::write(
            root.join("src/main/java/com/x/App.java"),
            "package com.x;\nimport com.x.Helper;\npublic class App {\n  Helper h;\n}\n",
        )
        .unwrap();

        let we = move_workspace_edit(
            root,
            "src/main/java/com/x/Helper.java",
            "src/main/java/com/y/Helper.java",
        )
        .expect("java move");
        let s = we.to_string();
        assert!(s.contains("com.y.Helper"), "importer's import retargeted to the new FQN: {s}");
        // The moved file's package line is a membership ReplaceLine against its old content.
        assert!(s.contains("package com.y;"), "moved file's package declaration rewritten: {s}");
    }

    // M3: moving a DEFAULT-package file INTO a package must ADD the `package` declaration (an
    // InsertAt), not decline the whole move for lack of an existing `package` line.
    #[test]
    fn move_into_package_from_default_inserts_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("com/x")).unwrap();
        std::fs::write(root.join("Loose.java"), "public class Loose {}\n").unwrap();
        let m = JavaMoveModel(root);
        let edits =
            m.membership_edits("Loose.java", "com/x/Loose.java").expect("handled, not declined");
        assert!(
            edits.iter().any(|e| matches!(e, MembershipEdit::InsertAt { text, .. } if text.contains("package com.x;"))),
            "adds the destination package decl: {edits:?}"
        );
    }

    // A same-package rename (same directory) needs no package edit but still counts as a move
    // the engine handles: membership is an empty vec, not a decline.
    #[test]
    fn same_package_rename_has_empty_membership() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/main/java/com/x")).unwrap();
        std::fs::write(root.join("src/main/java/com/x/A.java"), "package com.x;\npublic class A {}\n").unwrap();
        let m = JavaMoveModel(root);
        assert_eq!(
            m.membership_edits("src/main/java/com/x/A.java", "src/main/java/com/x/B.java"),
            Some(Vec::new()),
            "same package: no package-line edit, but the move is still handled"
        );
    }

    // Import occurrences carry the deletion note + a rewrite span; the splice retargets the FQN.
    #[test]
    fn import_occurrence_is_noted_and_rewritable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/main/java/com/x")).unwrap();
        std::fs::write(root.join("src/main/java/com/x/Helper.java"), "package com.x;\npublic class Helper {}\n").unwrap();
        std::fs::write(root.join("src/main/java/com/x/App.java"), "package com.x;\nimport com.x.Helper;\n").unwrap();
        let m = JavaMoveModel(root);
        let content = "package com.x;\nimport com.x.Helper;\n";
        let occ: Vec<_> = m
            .ref_occurrences("src/main/java/com/x/App.java", content)
            .into_iter()
            .filter(|o| o.target == "src/main/java/com/x/Helper.java")
            .collect();
        assert_eq!(occ.len(), 1, "one import occurrence: {occ:?}");
        assert!(occ[0].note.as_deref().unwrap().contains("unresolved import"), "noted for deletion diagnostics");
        let line = "import com.x.Helper;";
        let spans: Vec<_> = occ.iter().filter_map(|o| o.span).collect();
        assert_eq!(splice_spans(line, &spans, "com.y.Helper"), "import com.y.Helper;", "FQN retargeted");
    }

    // M1: a fully-qualified name inside a STRING or COMMENT is never a rewrite target (a rewrite
    // there still compiles, so the gate can't catch it); a real code mention still is.
    #[test]
    fn code_mentions_in_strings_and_comments_are_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/main/java/com/x")).unwrap();
        std::fs::write(root.join("src/main/java/com/x/Helper.java"), "package com.x;\npublic class Helper {}\n")
            .unwrap();
        let m = JavaMoveModel(root);
        let content = "package com.x;\n\
public class App {\n\
  // see com.x.Helper for details\n\
  String name = \"com.x.Helper\";\n\
  com.x.Helper h;\n\
}\n";
        let occ: Vec<_> = m
            .ref_occurrences("src/main/java/com/x/App.java", content)
            .into_iter()
            .filter(|o| o.target == "src/main/java/com/x/Helper.java")
            .collect();
        assert_eq!(occ.len(), 1, "only the real code mention, not the string/comment ones: {occ:?}");
        assert_eq!(occ[0].line, 4, "the surviving occurrence is the `com.x.Helper h;` on line 4 (0-based)");
    }
}
