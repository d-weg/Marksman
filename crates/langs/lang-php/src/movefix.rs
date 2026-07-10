//! movefix — the PHP §8 hooks behind the shared move engine, as a
//! [`ci_edit::moves::dotted`] instance (the generic dotted-name engine owns the control flow;
//! what is PHP about a move is the [`DottedSyntax`] scalars + the hooks below).
//!
//! phpactor's `willRenameFiles` IS engine-native for PHP moves (source-verified: its
//! `FileRenameHandler` returns a WorkspaceEdit rewriting the class's namespace + references)
//! and is preferred WHERE AVAILABLE — the same ordering lang-java settled for jdtls
//! (engine-native first, the syntactic fallback second, the gate over both). But phpactor is
//! often absent (a PHAR install, PHP-runtime-bound); these hooks are the runnable fallback that
//! gives PHP complete one-call moves without it.
//!
//! The three concerns, PHP edition:
//! - **file↔name**: `src/Foo/Bar.php` ↔ `App\Foo\Bar`, inverting the PSR-4 resolution the
//!   import graph already uses (one resolver, via `lang_fallback::php`).
//! - **references**: `use A\B\C;` declarations (aliases and a legal leading `\` handled) +
//!   fully-qualified `A\B\C` code mentions. A trailing `\` is a DEEPER namespace, never a
//!   reference to this class — the opposite of Java's trailing `.`.
//! - **membership**: the `namespace A\B;` line only — a class is a member of its namespace by
//!   the PSR-4 dir mapping; there is no per-file membership file.
//!
//! Best-effort BY DESIGN, exactly like the reference (`lang-rust::movefix`): a shape the model
//! doesn't understand returns `None`, the caller falls through to phpactor, and the PHPStan gate
//! rejects any rewrite that comes out wrong — the fallback can be incomplete, never silently wrong.
use ci_edit::moves::dotted::{self, DottedLang, DottedSyntax};
use ci_edit::moves::{MembershipEdit, MoveModel, RefOccurrence};
use lang_fallback::php;
use std::path::{Path, PathBuf};

/// The PHP [`MoveModel`]: FQCN references and namespace-declaration membership.
pub(crate) struct PhpMoveModel<'a>(pub(crate) &'a Path);

static PHP_SYNTAX: DottedSyntax = DottedSyntax {
    sep: '\\',
    import_kw: "use ",
    import_modifiers: &["function ", "const "],
    import_stops: &[';', ','],
    import_alias_kw: Some(" as "),
    reject_import_suffix: None,
    // A grouped `use A\B\{C, D};` names a package, not one file — only single-class uses
    // retarget.
    reject_import_containing: Some('{'),
    // A trailing `\` means a longer FQCN (`App\Foo\Bar`) — NOT a reference to `App\Foo`
    // (unlike Java's trailing `.`, which stays a member access on the class).
    trailing_sep_refs_target: false,
    // `\App\Foo::bar()` — a fully-qualified mention may start with the separator.
    run_may_start_with_sep: true,
    source_ext: ".php",
};

impl DottedLang for PhpMoveModel<'_> {
    fn syntax(&self) -> &'static DottedSyntax {
        &PHP_SYNTAX
    }

    fn root(&self) -> &Path {
        self.0
    }

    fn path_to_name(&self, rel: &str) -> Option<String> {
        php::file_to_fqcn(self.0, rel)
    }

    fn resolve_name(&self, _from_rel: &str, name: &str) -> Option<PathBuf> {
        php::resolve_use(self.0, name)
    }

    /// Finding the EXISTING declaration is `php::namespace_decl` — one shared scanner, §7.
    fn decl_line(&self, content: &str) -> Option<usize> {
        php::namespace_decl(content).map(|(idx, _)| idx)
    }

    fn render_decl(&self, ns: &str) -> String {
        format!("namespace {ns};")
    }

    /// Right after the `<?php` opening tag, else line 0.
    fn insert_line(&self, content: &str) -> usize {
        for (i, line) in content.lines().enumerate() {
            if line.trim_start().starts_with("<?php") {
                return i + 1;
            }
        }
        0
    }

    /// Mask via tree-sitter (exact for heredocs, nowdocs, interpolated strings) — M1.
    fn masked_spans(&self, content: &str) -> Vec<(usize, usize)> {
        lang_fallback::string_comment_spans(lang_fallback::FbLang::Php, content)
    }

    fn deletion_note(&self, fqcn: &str, target: &str) -> String {
        format!("unresolved use `{fqcn}` — {target} is deleted/moved by this batch; update the use")
    }
}

impl MoveModel for PhpMoveModel<'_> {
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
        rel.ends_with(PHP_SYNTAX.source_ext)
    }
}

/// The move's `WorkspaceEdit` over [`PhpMoveModel`], or `None` when the move shape is outside
/// what this fallback understands (phpactor, then the PHPStan gate, still protect the commit).
pub(crate) fn move_workspace_edit(root: &Path, from: &str, to: &str) -> Option<serde_json::Value> {
    ci_edit::moves::move_workspace_edit(root, from, to, &PhpMoveModel(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_edit::moves::splice_spans;

    /// The historical local fn, now the generic engine's boundary checker over PHP syntax —
    /// kept so the pinned span tests read (and assert) exactly what they always did.
    fn fqcn_spans(line: &str, fqcn: &str) -> Vec<(usize, usize)> {
        dotted::name_spans(line, fqcn, &PHP_SYNTAX)
    }

    fn psr4_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/App/Sub")).unwrap();
        std::fs::write(
            root.join("composer.json"),
            "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/App/\" } } }\n",
        )
        .unwrap();
        std::fs::write(root.join("src/App/Helper.php"), "<?php\nnamespace App;\nclass Helper {}\n").unwrap();
        dir
    }

    #[test]
    fn fqcn_spans_are_token_bounded() {
        assert_eq!(fqcn_spans("use App\\Foo;", "App\\Foo"), vec![(4, 11)]);
        // Never inside a longer FQCN (trailing `\` = deeper sub-namespace, not this class).
        assert_eq!(fqcn_spans("use App\\FooBar;", "App\\Foo"), vec![]);
        assert_eq!(fqcn_spans("use App\\Foo\\Bar;", "App\\Foo"), vec![]);
        // A `::` static access DOES reference the class.
        assert_eq!(fqcn_spans("x = App\\Foo::bar() + My\\App\\Foo;", "App\\Foo"), vec![(4, 11)]);
    }

    #[test]
    fn file_to_ref_inverts_psr4() {
        let dir = psr4_repo();
        let m = PhpMoveModel(dir.path());
        assert_eq!(m.file_to_ref("src/App/Helper.php").as_deref(), Some("App\\Helper"));
        assert_eq!(m.file_to_ref("README.md"), None);
    }

    // The move assembly: an importer's `use` line retargets to the new FQCN, and the moved
    // file's own `namespace` declaration is rewritten to the destination namespace.
    #[test]
    fn cross_namespace_move_rewrites_use_and_namespace_line() {
        let dir = psr4_repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/App/Sub")).unwrap();
        std::fs::write(
            root.join("src/App/Consumer.php"),
            "<?php\nnamespace App;\nuse App\\Helper;\nclass Consumer {\n  private Helper $h;\n}\n",
        )
        .unwrap();

        let we = move_workspace_edit(root, "src/App/Helper.php", "src/App/Sub/Helper.php").expect("php move");
        let s = we.to_string();
        assert!(s.contains("App\\\\Sub\\\\Helper"), "importer's use retargeted to the new FQCN: {s}");
        assert!(s.contains("namespace App\\\\Sub;"), "moved file's namespace declaration rewritten: {s}");
    }

    #[test]
    fn same_namespace_rename_has_empty_membership() {
        let dir = psr4_repo();
        let m = PhpMoveModel(dir.path());
        assert_eq!(
            m.membership_edits("src/App/Helper.php", "src/App/Renamed.php"),
            Some(Vec::new()),
            "same namespace: no namespace-line edit, but the move is still handled"
        );
    }

    // Use occurrences carry the deletion note + a rewrite span; the splice retargets the FQCN.
    #[test]
    fn use_occurrence_is_noted_and_rewritable() {
        let dir = psr4_repo();
        let root = dir.path();
        let m = PhpMoveModel(root);
        let content = "<?php\nnamespace App;\nuse App\\Helper;\n";
        let occ: Vec<_> = m
            .ref_occurrences("src/App/Consumer.php", content)
            .into_iter()
            .filter(|o| o.target == "src/App/Helper.php")
            .collect();
        assert_eq!(occ.len(), 1, "one use occurrence: {occ:?}");
        assert!(occ[0].note.as_deref().unwrap().contains("unresolved use"), "noted for deletion diagnostics");
        let line = "use App\\Helper;";
        let spans: Vec<_> = occ.iter().filter_map(|o| o.span).collect();
        assert_eq!(splice_spans(line, &spans, "App\\Sub\\Helper"), "use App\\Sub\\Helper;", "FQCN retargeted");
    }

    // M2: a legal leading `\` in a `use` (`use \App\Helper;`) must STILL be rewritten and noted —
    // the boundary checker reads the `\` as a longer-name boundary, so the use branch locates it
    // directly.
    #[test]
    fn leading_backslash_use_is_rewritten_and_noted() {
        let dir = psr4_repo();
        let m = PhpMoveModel(dir.path());
        let content = "<?php\nnamespace App;\nuse \\App\\Helper;\n";
        let occ: Vec<_> = m
            .ref_occurrences("src/App/Consumer.php", content)
            .into_iter()
            .filter(|o| o.target == "src/App/Helper.php")
            .collect();
        assert_eq!(occ.len(), 1, "the leading-`\\` use still yields an occurrence: {occ:?}");
        assert!(occ[0].note.is_some(), "noted for deletion diagnostics");
        let line = "use \\App\\Helper;";
        let spans: Vec<_> = occ.iter().filter_map(|o| o.span).collect();
        assert_eq!(
            splice_spans(line, &spans, "App\\Sub\\Helper"),
            "use \\App\\Sub\\Helper;",
            "FQCN retargeted behind the leading `\\`"
        );
    }

    // M3: moving a GLOBAL-namespace file INTO a namespace must ADD the declaration (an InsertAt),
    // not decline the whole move for lack of an existing `namespace` line.
    #[test]
    fn move_into_namespace_from_global_inserts_declaration() {
        let dir = psr4_repo();
        let root = dir.path();
        std::fs::write(root.join("src/App/Loose.php"), "<?php\nclass Loose {}\n").unwrap();
        let m = PhpMoveModel(root);
        let edits = m
            .membership_edits("src/App/Loose.php", "src/App/Sub/Loose.php")
            .expect("handled, not declined");
        assert!(
            edits.iter().any(|e| matches!(e, MembershipEdit::InsertAt { text, .. } if text.contains("namespace App\\Sub;"))),
            "adds the destination namespace decl: {edits:?}"
        );
    }

    // M1: a FQCN inside a STRING or COMMENT is never a rewrite target (a rewrite there still
    // parses/compiles, so the PHPStan gate can't catch it); a real code mention still is.
    #[test]
    fn code_mentions_in_strings_and_comments_are_not_rewritten() {
        let dir = psr4_repo();
        let m = PhpMoveModel(dir.path());
        let content = "<?php\n\
namespace App;\n\
// see App\\Helper for details\n\
$c = \"App\\Helper\";\n\
$h = new App\\Helper();\n";
        let occ: Vec<_> = m
            .ref_occurrences("src/App/Consumer.php", content)
            .into_iter()
            .filter(|o| o.target == "src/App/Helper.php")
            .collect();
        assert_eq!(occ.len(), 1, "only the real `new App\\Helper()` code mention: {occ:?}");
        assert_eq!(occ[0].line, 4, "the surviving occurrence is on line 4 (0-based)");
    }
}
