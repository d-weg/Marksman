//! movefix — the PHP §8 hooks ([`ci_edit::moves::MoveModel`]) behind the shared move engine.
//!
//! phpactor's `willRenameFiles` IS engine-native for PHP moves (source-verified: its
//! `FileRenameHandler` returns a WorkspaceEdit rewriting the class's namespace + references)
//! and is preferred WHERE AVAILABLE — the same ordering lang-java settled for jdtls
//! (engine-native first, the syntactic fallback second, the gate over both). But phpactor is
//! often absent (a PHAR install, PHP-runtime-bound); these hooks are the runnable fallback that
//! gives PHP complete one-call moves without it. What is PHP about a move lives HERE; the file
//! walking / span splicing / WorkspaceEdit assembly is `ci_edit::moves`.
//!
//! The three concerns, PHP edition:
//! - **`file_to_ref`**: path → fully-qualified class name (`src/Foo/Bar.php` → `App\Foo\Bar`),
//!   inverting the PSR-4 resolution the import graph already uses (one resolver, via
//!   `lang_fallback::php`).
//! - **`ref_occurrences`**: `use A\B\C;` declarations (span-rewritable, noted for deletion
//!   diagnostics) plus fully-qualified `A\B\C` mentions in code (rewrite-only — the gate catches
//!   a stranded FQCN the imports don't cover). Each resolves through the shared PSR-4 resolver
//!   so a reference to a batch-deleted class is a diagnostic.
//! - **`membership_edits`**: rewrite the `namespace A\B;` declaration to match the new directory.
//!   PHP has NO per-file membership file (a class is a member of its namespace by the PSR-4 dir
//!   mapping) — so this hook is namespace-line-only by design.
//!
//! Best-effort BY DESIGN, exactly like the reference (`lang-rust::movefix`): a shape the model
//! doesn't understand returns `None`, the caller falls through to phpactor, and the PHPStan gate
//! rejects any rewrite that comes out wrong — the fallback can be incomplete, never silently wrong.
use ci_edit::moves::{MembershipEdit, MoveModel, RefOccurrence};
use lang_fallback::php;
use std::path::Path;

/// The PHP [`MoveModel`]: FQCN references and namespace-declaration membership.
pub(crate) struct PhpMoveModel<'a>(pub(crate) &'a Path);

/// Byte spans of the exact FQCN token `fqcn` in `line`, bounded so `App\Foo` never matches
/// inside `App\FooBar` or `My\App\Foo`. Boundary chars: a PHP identifier segment is
/// `[A-Za-z0-9_]` and namespace separator `\`. The rule is ASYMMETRIC: a LEADING identifier
/// char or `\` means we sit inside a longer, more-qualified name (not this class); a TRAILING
/// identifier char means a longer simple name, but a trailing `\` (a deeper sub-namespace) or
/// `::` (a static member access) still references this class.
fn fqcn_spans(line: &str, fqcn: &str) -> Vec<(usize, usize)> {
    let before_ext = |c: char| c.is_alphanumeric() || c == '_' || c == '\\';
    let after_ext = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = line[from..].find(fqcn) {
        let at = from + pos;
        let end = at + fqcn.len();
        let before_ok = at == 0 || !line[..at].chars().next_back().is_some_and(before_ext);
        // A trailing `\` means a longer FQCN (`App\Foo\Bar`) — NOT a reference to `App\Foo`, so
        // reject it (unlike Java's trailing `.` which stays a member access on the class).
        let after_ok = line[end..].chars().next().is_none_or(|c| !after_ext(c) && c != '\\');
        if before_ok && after_ok {
            out.push((at, end));
        }
        from = end;
    }
    out
}

impl MoveModel for PhpMoveModel<'_> {
    fn file_to_ref(&self, rel: &str) -> Option<String> {
        php::file_to_fqcn(self.0, rel)
    }

    /// Two occurrence kinds per line:
    /// - `use A\B\C;` declarations: the FQCN is a span-rewritable reference resolving to a file
    ///   (noted for the deletion pass — a `use` of a deleted class is the unresolved symbol
    ///   PHPStan reports, but the anchored reject-recipe wants it too).
    /// - fully-qualified `A\B\C` mentions elsewhere (`new App\Foo()`, `App\Foo::bar()`): the
    ///   longest backslash-prefix resolving to a source file is a rewrite span (`note: None` —
    ///   the gate catches a stranded expression FQCN; this just keeps a move complete).
    fn ref_occurrences(&self, _rel: &str, content: &str) -> Vec<RefOccurrence> {
        let mut out = Vec::new();
        // A FQCN inside a STRING or COMMENT is not a reference to rewrite: a rewrite there still
        // parses/compiles, so the PHPStan gate can't catch it. Mask those extents via tree-sitter
        // (exact for heredocs, nowdocs, interpolated strings) and skip any code-mention run that
        // starts inside one (M1). `use` lines never sit in strings, so that branch stays unmasked.
        let masked = lang_fallback::string_comment_spans(lang_fallback::FbLang::Php, content);
        let line_starts = lang_fallback::line_start_offsets(content);
        let in_string_or_comment = |abs: usize| masked.iter().any(|&(s, e)| abs >= s && abs < e);
        for (i, line) in content.lines().enumerate() {
            let t = line.trim_start();
            // `use [function|const] A\B\C [as X];` — take the FQCN before any alias.
            let use_fqcn = t
                .strip_prefix("use ")
                .map(|r| {
                    let r = r.trim_start();
                    r.strip_prefix("function ").or_else(|| r.strip_prefix("const ")).map(str::trim_start).unwrap_or(r)
                })
                .and_then(|r| r.split([';', ',']).next())
                .map(|r| r.split(" as ").next().unwrap_or(r).trim().trim_start_matches('\\'));
            if let Some(fqcn) = use_fqcn {
                // A grouped `use A\B\{C, D};` names a package, not one file — leave those to the
                // moved file staying resolvable by its PSR-4 dir; only single-class uses retarget.
                if !fqcn.is_empty() && !fqcn.contains('{') {
                    if let Some(target) = php::resolve_use(self.0, fqcn) {
                        let target = target.to_string_lossy().replace('\\', "/");
                        let note = format!(
                            "unresolved use `{fqcn}` — {target} is deleted/moved by this batch; update the use"
                        );
                        // A `use` line names exactly this FQCN once — locate it directly. Do NOT
                        // reuse the boundary-sensitive `fqcn_spans` here: a legal leading `\`
                        // (`use \App\Foo;`) reads as a longer-name boundary and yields zero spans,
                        // losing both the rewrite AND the deletion diagnostic (M2).
                        if let Some(at) = line.find(fqcn) {
                            out.push(RefOccurrence {
                                line: i,
                                span: Some((at, at + fqcn.len())),
                                target: target.clone(),
                                note: Some(note.clone()),
                            });
                        }
                    }
                }
                continue; // a use line carries no other FQCN references worth scanning
            }
            // Fully-qualified references in code: walk backslash-dotted runs, resolve the longest
            // prefix that lands on a source file. Rewrite-only (the gate owns correctness).
            for (start, dotted) in backslash_runs(line) {
                if in_string_or_comment(line_starts[i] + start) {
                    continue; // a FQCN inside a string/comment is never a rewrite target (M1)
                }
                let segs: Vec<&str> = dotted.split('\\').filter(|s| !s.is_empty()).collect();
                for n in (1..=segs.len()).rev() {
                    let prefix = segs[..n].join("\\");
                    if let Some(target) = php::resolve_use(self.0, &prefix) {
                        let target = target.to_string_lossy().replace('\\', "/");
                        // The rewrite span is the resolved prefix as it appears at `start` (the
                        // run may carry a leading `\`; anchor on the prefix's own offset).
                        if let Some((s, e)) = fqcn_spans(&line[start..], &prefix).into_iter().next() {
                            out.push(RefOccurrence { line: i, span: Some((start + s, start + e)), target, note: None });
                        }
                        break; // longest resolving prefix wins
                    }
                }
            }
        }
        out
    }

    /// Namespace-line-only membership (PHP has no `mod.rs`): moving `from`→`to` across
    /// namespaces rewrites the moved file's own `namespace N;` line to the destination
    /// namespace. Same-namespace moves (a pure rename in the same PSR-4 dir) need no namespace
    /// edit — an empty vec, not `None`, so the engine still rewrites `use` importers. `None`
    /// only when either path isn't a resolvable `.php` FQCN (the engine declines, the caller
    /// falls through to phpactor).
    fn membership_edits(&self, from: &str, to: &str) -> Option<Vec<MembershipEdit>> {
        let from_fqcn = php::file_to_fqcn(self.0, from)?;
        let to_fqcn = php::file_to_fqcn(self.0, to)?;
        let from_ns = fqcn_namespace(&from_fqcn);
        let to_ns = fqcn_namespace(&to_fqcn);
        if from_ns == to_ns {
            return Some(Vec::new()); // same namespace: importers rewrite, the namespace line stays
        }
        // The moved file's own `namespace` declaration must name the destination namespace. As
        // in the Java model, the edit rides on the moved file (`from`), which the engine renders
        // against `from`'s content BEFORE the file moves. One shared namespace scanner
        // (`php::namespace_decl`) — no divergent reimplementation (§7).
        let content = std::fs::read_to_string(self.0.join(from)).ok()?;
        match php::namespace_decl(&content) {
            Some((idx, _)) => {
                // Moved into the global namespace → drop the declaration (empty replacement line).
                let new_line = if to_ns.is_empty() { String::new() } else { format!("namespace {to_ns};") };
                Some(vec![MembershipEdit::ReplaceLine { file: from.to_string(), line: idx, new_text: new_line }])
            }
            None if to_ns.is_empty() => Some(Vec::new()), // global → global: nothing to declare
            None => {
                // No namespace today, moving INTO one: ADD the declaration after the `<?php` tag.
                // A missing InsertAt used to make the engine decline the whole move (M3).
                let at = php_insert_line(&content);
                Some(vec![MembershipEdit::InsertAt {
                    file: from.to_string(),
                    line: at,
                    text: format!("namespace {to_ns};\n"),
                }])
            }
        }
    }

    fn is_source(&self, rel: &str) -> bool {
        rel.ends_with(".php")
    }
}

/// The namespace part of an FQCN (`App\Foo\Bar` → `App\Foo`; a top-level `Bar` → `""`).
fn fqcn_namespace(fqcn: &str) -> String {
    match fqcn.rfind('\\') {
        Some(i) => fqcn[..i].to_string(),
        None => String::new(),
    }
}

/// The line to insert a `namespace` declaration at: right after the `<?php` opening tag, else
/// line 0. (Finding the EXISTING declaration is `php::namespace_decl` — one shared scanner, §7.)
fn php_insert_line(content: &str) -> usize {
    for (i, line) in content.lines().enumerate() {
        if line.trim_start().starts_with("<?php") {
            return i + 1;
        }
    }
    0
}

/// Maximal `[A-Za-z0-9_\\]` runs in `line` that contain a `\` and start on an identifier char or
/// a leading `\` — the candidate FQCN references (`App\Foo`, `\App\Foo::bar()` gives `App\Foo`).
/// Each is `(start byte, text)`; a leading `\` is kept off the returned run so the prefix anchors
/// on the first identifier.
fn backslash_runs(line: &str) -> Vec<(usize, String)> {
    let ext = |c: char| c.is_alphanumeric() || c == '_' || c == '\\';
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_alphabetic() || c == '_' || c == '\\' {
            let start = i;
            while i < line.len() && line[i..].chars().next().is_some_and(ext) {
                i += line[i..].chars().next().unwrap().len_utf8();
            }
            let run = line[start..i].trim_matches('\\');
            if run.contains('\\') {
                // Anchor on `run`'s own start (past any leading `\`).
                let run_start = start + (line[start..i].len() - line[start..i].trim_start_matches('\\').len());
                out.push((run_start, run.to_string()));
            }
        } else {
            i += c.len_utf8();
        }
    }
    out
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
    // fqcn_spans reads the `\` as a longer-name boundary, so the use branch locates it directly.
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
