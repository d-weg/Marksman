//! movefix — the Java §8 hooks behind the shared move engine, as a
//! [`ci_edit::moves::dotted`] instance (the generic dotted-name engine owns the control flow;
//! what is JAVA about a move is the [`DottedSyntax`] scalars + the hooks below).
//!
//! jdtls's `willRenameFiles` IS engine-native for Java moves (it rewrites the package decl AND
//! importers) and is preferred WHERE AVAILABLE — the exact ordering lang-rust settled for
//! rust-analyzer (engine-native first, the syntactic fallback second, the gate over both). But
//! jdtls is push-diagnostics-only, minutes-cold, and often absent; these hooks are the runnable
//! fallback that gives Java complete one-call moves without it.
//!
//! The three concerns, Java edition:
//! - **file↔name**: `src/main/java/com/x/A.java` ↔ `com.x.A`, inverting the source-root
//!   resolution the import graph already uses (one resolver, via `lang_fallback::java`).
//! - **references**: `import a.b.C;` declarations + fully-qualified `a.b.C` code mentions.
//!   Java's trailing `.` IS a reference (`com.x.A.f()` — member access), unlike PHP's `\`.
//! - **membership**: the `package p;` line only — Java has NO per-file membership file like
//!   Rust's `mod.rs`; a class is a member of its package by living in the matching directory.
//!
//! Best-effort BY DESIGN, exactly like the reference (`lang-rust::movefix`): a shape the model
//! doesn't understand returns `None`, the caller falls through to jdtls, and the javac gate
//! rejects any rewrite that comes out wrong — the fallback can be incomplete, never silently
//! wrong.
use ci_edit::moves::dotted::{self, DottedLang, DottedSyntax};
use ci_edit::moves::{MembershipEdit, MoveModel, RefOccurrence};
use lang_fallback::java;
use std::path::{Path, PathBuf};

/// The Java [`MoveModel`]: FQN references and package-declaration membership.
pub(crate) struct JavaMoveModel<'a>(pub(crate) &'a Path);

static JAVA_SYNTAX: DottedSyntax = DottedSyntax {
    sep: '.',
    import_kw: "import ",
    import_modifiers: &["static "],
    import_stops: &[';'],
    import_alias_kw: None,
    // On-demand `import a.b.*;` resolves to the package dir, not one file — only single-type
    // imports name a file to retarget.
    reject_import_suffix: Some('*'),
    reject_import_containing: None,
    // A trailing `.` is member access on the class (`com.x.A.f()`) or a nested type
    // (`com.x.A.B`) — both DO reference `com.x.A`.
    trailing_sep_refs_target: true,
    run_may_start_with_sep: false,
    source_ext: ".java",
};

impl DottedLang for JavaMoveModel<'_> {
    fn syntax(&self) -> &'static DottedSyntax {
        &JAVA_SYNTAX
    }

    fn root(&self) -> &Path {
        self.0
    }

    fn path_to_name(&self, rel: &str) -> Option<String> {
        java::file_to_fqn(self.0, rel)
    }

    fn resolve_name(&self, from_rel: &str, name: &str) -> Option<PathBuf> {
        java::resolve_import(self.0, from_rel, name)
    }

    /// Finding the EXISTING declaration is `java::package_decl` — one shared scanner, §7.
    fn decl_line(&self, content: &str) -> Option<usize> {
        java::package_decl(content).map(|(idx, _)| idx)
    }

    fn render_decl(&self, pkg: &str) -> String {
        format!("package {pkg};")
    }

    /// Before the first real statement (skipping a leading license comment / blank lines).
    fn insert_line(&self, content: &str) -> usize {
        for (i, line) in content.lines().enumerate() {
            let t = line.trim_start();
            if t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
                continue;
            }
            return i;
        }
        0
    }

    /// Mask via tree-sitter (exact for block comments, text blocks, escaped strings) — M1.
    fn masked_spans(&self, content: &str) -> Vec<(usize, usize)> {
        lang_fallback::string_comment_spans(lang_fallback::FbLang::Java, content)
    }

    fn deletion_note(&self, fqn: &str, target: &str) -> String {
        format!("unresolved import `{fqn}` — {target} is deleted/moved by this batch; update the import")
    }
}

impl MoveModel for JavaMoveModel<'_> {
    fn file_to_ref(&self, rel: &str) -> Option<String> {
        self.path_to_name(rel)
    }

    fn ref_occurrences(&self, rel: &str, content: &str) -> Vec<RefOccurrence> {
        dotted::ref_occurrences(self, rel, content)
    }

    fn membership_edits(&self, from: &str, to: &str) -> Option<Vec<MembershipEdit>> {
        dotted::membership_edits(self, from, to)
    }

    fn is_source(&self, rel: &str) -> bool {
        rel.ends_with(JAVA_SYNTAX.source_ext)
    }
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

    /// The historical local fn, now the generic engine's boundary checker over Java syntax —
    /// kept so the pinned span tests read (and assert) exactly what they always did.
    fn fqn_spans(line: &str, fqn: &str) -> Vec<(usize, usize)> {
        dotted::name_spans(line, fqn, &JAVA_SYNTAX)
    }

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
