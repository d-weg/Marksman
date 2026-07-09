//! movefix — the Rust §8 hooks behind the shared move engine, covering the syntactic
//! module-move rewrite rust-analyzer doesn't provide.
//!
//! ra's `willRenameFiles` rewrites same-directory renames but returns NOTHING for a move
//! into a submodule (`src/a.rs` → `src/d/a.rs`), leaving the `mod` declaration and every
//! `crate::a::…` path dangling — the agent then has to finish the move by hand (bench
//! `move-rust`: +10 turns of manual edits). The rewrite mechanics (file walking, span
//! edits, CreateFile ops, WorkspaceEdit assembly) are `ci_edit::moves` — the generic
//! engine extracted with THIS module as the reference semantics. What lives here is only
//! what is Rust about a move ([`RustMoveModel`]): module paths for files (`file_to_ref`),
//! `crate::`-chain / bare-head / `mod`-decl occurrences (`ref_occurrences`), and `mod`
//! declaration maintenance (`membership_edits`). The same occurrences feed the gate's
//! deleted-reference diagnostics (see `gate.rs`).
//!
//! Best-effort BY DESIGN: it handles crate-root moves/renames of leaf modules (the common
//! agent operations); anything it misses is caught by the type-check gate, which rejects
//! with named sites — the fallback can be incomplete, it can never be silently wrong.
use ci_edit::moves::{MembershipEdit, MoveModel, RefOccurrence};
use serde_json::Value;
use std::path::Path;

use crate::graph::resolve_mod;

/// Module path segments for a `src/…` rs file, e.g. `src/text/tokenize.rs` → `["text",
/// "tokenize"]`; `None` for non-src paths, crate roots (lib/main), and `mod.rs` (moving a
/// directory module is out of scope).
fn mod_segs(rel: &str) -> Option<Vec<String>> {
    let stem = rel.strip_prefix("src/")?.strip_suffix(".rs")?;
    if stem == "lib" || stem == "main" || stem.ends_with("/mod") {
        return None;
    }
    Some(stem.split('/').map(str::to_string).collect())
}

/// The file that must DECLARE module `segs` (its parent module file): `src/lib.rs` for
/// top-level modules, else `src/<parent>.rs` or `src/<parent>/mod.rs` — whichever exists.
/// The bool is true when the parent module file itself is missing (must be created).
fn parent_decl_file(root: &Path, segs: &[String]) -> (String, bool) {
    if segs.len() == 1 {
        let lib = if root.join("src/lib.rs").is_file() { "src/lib.rs" } else { "src/main.rs" };
        return (lib.to_string(), false);
    }
    let parent = segs[..segs.len() - 1].join("/");
    let flat = format!("src/{parent}.rs");
    let modrs = format!("src/{parent}/mod.rs");
    if root.join(&flat).is_file() {
        (flat, false)
    } else if root.join(&modrs).is_file() {
        (modrs, false)
    } else {
        (modrs, true) // create the mod.rs form
    }
}

/// The `[pub ]mod <name>;` declaration on one line → (visibility prefix e.g. `"pub "`,
/// name). File-module decls only — inline `mod x { … }` has no `;` and no file to move.
fn mod_decl_on(line: &str) -> Option<(&'static str, &str)> {
    let t = line.trim_start();
    let (vis, rest) = match t.strip_prefix("pub ") {
        Some(r) => ("pub ", r.trim_start()),
        None => ("", t),
    };
    let decl = rest.strip_prefix("mod ")?;
    let name = decl.trim_end().strip_suffix(';')?.trim();
    Some((vis, name))
}

/// Find the `[pub] mod <name>;` declaration line in `content`. Returns (line index, line,
/// visibility prefix e.g. `"pub "`).
fn find_mod_decl(content: &str, name: &str) -> Option<(usize, String, String)> {
    for (i, line) in content.lines().enumerate() {
        if let Some((vis, decl_name)) = mod_decl_on(line) {
            if decl_name == name {
                return Some((i, line.to_string(), vis.to_string()));
            }
        }
    }
    None
}

/// Token-bounded byte spans of bare `<head>::` path heads in `line`: `tokenize::x` when
/// `head` is `tokenize`, but never inside a longer identifier and never when qualified
/// (`crate::tokenize::x` — the preceding `:` rules it out; the chain scan owns that form).
fn bare_head_spans(line: &str, head: &str) -> Vec<(usize, usize)> {
    let needle = format!("{head}::");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = line[from..].find(&needle) {
        let at = from + pos;
        let before_ok = at == 0
            || !line[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':');
        if before_ok {
            out.push((at, at + head.len()));
        }
        from = at + needle.len();
    }
    out
}

/// The Rust [`MoveModel`]: how `.rs` files are referenced (`crate::` paths, bare in-scope
/// heads) and declared members (`mod x;`), for both the move rewriter and the gate's
/// deleted-reference diagnostics.
pub(crate) struct RustMoveModel<'a>(pub(crate) &'a Path);

impl MoveModel for RustMoveModel<'_> {
    fn file_to_ref(&self, rel: &str) -> Option<String> {
        mod_segs(rel).map(|s| s.join("::"))
    }

    /// Three occurrence kinds per line, mirroring what the pre-extraction implementations
    /// scanned:
    /// - `crate::a::b::…` chains: EVERY prefix is a candidate module file in both layouts
    ///   (`src/a/b.rs`, `src/a/b/mod.rs`) — candidates by construction, never disk-checked,
    ///   because deletion diagnostics run while the batch's deletions are already staged
    ///   off disk (E0432 class). Span-rewritable when the path text is contiguous.
    /// - `mod x;` declarations resolving to a file (E0583 class, decl side) — membership
    ///   territory, never span-rewritten.
    /// - bare `x::…` heads for modules THIS file declares (there the module is in scope
    ///   unqualified) — rewrite-only (`note: None`): rustc reports the stranded-import
    ///   classes above; bare expression paths are the type-checker's to catch.
    fn ref_occurrences(&self, rel: &str, content: &str) -> Vec<RefOccurrence> {
        let declared: Vec<(String, String)> = content
            .lines()
            .filter_map(|l| mod_decl_on(l).map(|(_, name)| name.to_string()))
            .filter_map(|name| {
                resolve_mod(self.0, rel, &name)
                    .map(|t| (name, t.to_string_lossy().replace('\\', "/")))
            })
            .collect();
        let mut out = Vec::new();
        for (i, line) in content.lines().enumerate() {
            // `crate::a::b::…` — walk the segment chain; any prefix landing on a deleted
            // module file is a stranded reference, and the prefix naming the moved file is
            // the rewrite span.
            let mut base = 0usize;
            let mut rest = line;
            while let Some(pos) = rest.find("crate::") {
                let chain_start = base + pos + 7;
                let tail = &rest[pos + 7..];
                let segs: Vec<&str> = tail
                    .split("::")
                    .map(|s| s.trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '_')))
                    .take_while(|s| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_'))
                    .collect();
                for n in 1..=segs.len() {
                    let path = segs[..n].join("::");
                    let span = (line.get(chain_start..chain_start + path.len()) == Some(path.as_str()))
                        .then_some((chain_start, chain_start + path.len()));
                    let base_path = segs[..n].join("/");
                    for target in [format!("src/{base_path}.rs"), format!("src/{base_path}/mod.rs")] {
                        let note = format!(
                            "unresolved import `crate::{path}` — {target} is deleted/moved by this batch (E0432); update the path"
                        );
                        out.push(RefOccurrence { line: i, span, target, note: Some(note) });
                    }
                }
                base += pos + 7;
                rest = &rest[pos + 7..];
            }
            // `mod x;` decls (E0583-class, decl side).
            let t = line.trim_start();
            let decl = t.strip_prefix("pub ").unwrap_or(t);
            if let Some(m) = decl.strip_prefix("mod ") {
                if let Some(name) = m.trim_end().strip_suffix(';') {
                    if let Some(target) = resolve_mod(self.0, rel, name.trim()) {
                        let target = target.to_string_lossy().replace('\\', "/");
                        let note = format!(
                            "`mod {}` points at {target}, which this batch deletes/moves (E0583); update or remove the declaration",
                            name.trim()
                        );
                        out.push(RefOccurrence { line: i, span: None, target, note: Some(note) });
                    }
                }
            }
            // Bare heads of the modules this file declares.
            for (name, target) in &declared {
                for span in bare_head_spans(line, name) {
                    out.push(RefOccurrence { line: i, span: Some(span), target: target.clone(), note: None });
                }
            }
        }
        out
    }

    /// `mod` declaration maintenance. Supported shapes: rename in place (any depth), and a
    /// TOP-LEVEL module moving one level down (`src/a.rs` → `src/d/a.rs`). Deeper/lateral
    /// moves return `None` — the agent's manual path, still gate-protected.
    fn membership_edits(&self, from: &str, to: &str) -> Option<Vec<MembershipEdit>> {
        let from_segs = mod_segs(from)?;
        let to_segs = mod_segs(to)?;
        let old_name = from_segs.last()?.clone();
        let new_name = to_segs.last()?.clone();

        // The OLD declaring file: drop (or repurpose) the `mod <old>;` line.
        let (old_parent, old_parent_missing) = parent_decl_file(self.0, &from_segs);
        if old_parent_missing {
            return None; // pathological: the moved module was never declared
        }
        let old_parent_content = std::fs::read_to_string(self.0.join(&old_parent)).ok()?;
        let (decl_idx, _decl_line, vis) = find_mod_decl(&old_parent_content, &old_name)?;

        let (new_parent, new_parent_missing) = parent_decl_file(self.0, &to_segs);
        let mut out: Vec<MembershipEdit> = Vec::new();
        if new_parent == old_parent {
            // rename in place: `mod a;` → `mod b;`
            out.push(MembershipEdit::ReplaceLine {
                file: old_parent,
                line: decl_idx,
                new_text: format!("{vis}mod {new_name};"),
            });
            return Some(out);
        }
        if from_segs.len() != 1 || to_segs.len() != 2 {
            return None;
        }
        // The new top segment is declared where the old module was: repurpose the old decl
        // line when `d` isn't declared yet, else just delete it.
        let new_top = &to_segs[0];
        if find_mod_decl(&old_parent_content, new_top).is_some() {
            out.push(MembershipEdit::DeleteLine { file: old_parent.clone(), line: decl_idx });
        } else {
            out.push(MembershipEdit::ReplaceLine {
                file: old_parent.clone(),
                line: decl_idx,
                new_text: format!("{vis}mod {new_top};"),
            });
        }
        // The NEW parent module file declares the moved module.
        if new_parent_missing {
            out.push(MembershipEdit::CreateFile { file: new_parent, content: format!("pub mod {new_name};\n") });
        } else {
            let content = std::fs::read_to_string(self.0.join(&new_parent)).ok()?;
            if find_mod_decl(&content, &new_name).is_none() {
                out.push(MembershipEdit::InsertAt {
                    file: new_parent,
                    line: content.lines().count(),
                    text: format!("pub mod {new_name};\n"),
                });
            }
        }
        Some(out)
    }

    fn is_source(&self, rel: &str) -> bool {
        rel.ends_with(".rs") && !rel.starts_with("target/")
    }
}

/// The move's `WorkspaceEdit` (LSP `documentChanges`: CreateFile + TextEdits), or `None`
/// when the move shape is outside what this fallback understands (the gate still protects
/// the commit either way). Assembly is the shared §8 engine over [`RustMoveModel`].
pub(crate) fn move_workspace_edit(root: &Path, from: &str, to: &str) -> Option<Value> {
    ci_edit::moves::move_workspace_edit(root, from, to, &RustMoveModel(root))
}

/// The (parent_file, old_text, new_text) ReplaceInFile that DECLARES module `rel` in its
/// parent — for `create_file` of an undeclared module file: without the declaration the new
/// file is an orphan the crate never compiles, so the create op synthesizes this alongside
/// itself (bench-shaped agents always hand-write it; server-side it is one deterministic
/// edit). Insertion point: after the parent's LAST `mod` declaration, else after a leading
/// `//!` doc block, else before the first line. `None` when out of scope (not a module file,
/// parent missing, or already declared) — the caller simply doesn't synthesize.
pub(crate) fn declare_module_edit(root: &Path, rel: &str) -> Option<(String, String, String)> {
    let segs = mod_segs(rel)?;
    let name = segs.last()?.clone();
    let (parent, parent_missing) = parent_decl_file(root, &segs);
    if parent_missing {
        return None;
    }
    let content = std::fs::read_to_string(root.join(&parent)).ok()?;
    if find_mod_decl(&content, &name).is_some() {
        return None;
    }
    let decl = format!("pub mod {name};");
    let lines: Vec<&str> = content.lines().collect();
    // after the last existing mod decl…
    if let Some(last) = lines.iter().rposition(|l| {
        let t = l.trim_start();
        t.starts_with("mod ") && t.ends_with(';') || t.starts_with("pub mod ") && t.ends_with(';')
    }) {
        let anchor = lines[last];
        if content.matches(anchor).count() == 1 {
            return Some((parent, anchor.to_string(), format!("{anchor}
{decl}")));
        }
    }
    // …else after a leading //! block…
    if let Some(last_doc) = lines.iter().rposition(|l| l.trim_start().starts_with("//!")) {
        let anchor = lines[last_doc];
        if lines[..=last_doc].iter().all(|l| l.trim_start().starts_with("//!") || l.trim().is_empty())
            && content.matches(anchor).count() == 1
        {
            return Some((parent, anchor.to_string(), format!("{anchor}
{decl}")));
        }
    }
    // …else before the first line (unique by construction only if the file is non-empty).
    let first = lines.first()?;
    if content.matches(first).count() == 1 {
        return Some((parent, first.to_string(), format!("{decl}
{first}")));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_edit::moves::splice_spans;

    #[test]
    fn declare_module_edit_covers_the_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        // after the LAST mod decl
        std::fs::write(root.join("src/lib.rs"), "//! docs\npub mod a;\npub mod b;\n").unwrap();
        let (parent, old, new) = declare_module_edit(root, "src/zed.rs").expect("edit");
        assert_eq!(parent, "src/lib.rs");
        assert_eq!(old, "pub mod b;");
        assert_eq!(new, "pub mod b;\npub mod zed;");
        // already declared -> None
        assert!(declare_module_edit(root, "src/a.rs").is_none());
        // no decls: after the leading //! block
        std::fs::write(root.join("src/lib.rs"), "//! only docs\n\nfn main_ish() {}\n").unwrap();
        let (_, old, new) = declare_module_edit(root, "src/zed.rs").expect("edit");
        assert_eq!(old, "//! only docs");
        assert!(new.ends_with("pub mod zed;"));
        // non-module paths -> None
        assert!(declare_module_edit(root, "src/lib.rs").is_none());
        assert!(declare_module_edit(root, "README.md").is_none());
    }

    #[test]
    fn segs_and_parents() {
        assert_eq!(mod_segs("src/tokenize.rs"), Some(vec!["tokenize".into()]));
        assert_eq!(mod_segs("src/text/tokenize.rs"), Some(vec!["text".into(), "tokenize".into()]));
        assert_eq!(mod_segs("src/lib.rs"), None);
        assert_eq!(mod_segs("src/text/mod.rs"), None);
    }

    // The same contracts `rewrite_line` pinned pre-extraction, now as hook occurrences +
    // the shared splice: bare heads rewrite token-bounded (`myutil::` never matches a
    // `util` move), and `crate::` paths rewrite inside `use` chains.
    #[test]
    fn bare_rewrite_is_token_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/util.rs"), "pub fn one() {}\n").unwrap();
        std::fs::write(root.join("src/myutil.rs"), "pub fn two() {}\n").unwrap();
        let model = RustMoveModel(root);

        // Bare heads: only the DECLARED module's token-bounded `util::` head is a span.
        let content = "mod util;\nmod myutil;\n    util::one() + myutil::two()\n";
        let line = "    util::one() + myutil::two()";
        let spans: Vec<(usize, usize)> = model
            .ref_occurrences("src/lib.rs", content)
            .into_iter()
            .filter(|o| o.line == 2 && o.target == "src/util.rs")
            .filter_map(|o| o.span)
            .collect();
        assert_eq!(splice_spans(line, &spans, "core::util"), "    core::util::one() + myutil::two()");

        // Crate-qualified chains rewrite at the prefix naming the moved file.
        let line = "use crate::tokenize::tokenize;";
        let spans: Vec<(usize, usize)> = model
            .ref_occurrences("src/store.rs", "use crate::tokenize::tokenize;\n")
            .into_iter()
            .filter(|o| o.target == "src/tokenize.rs")
            .filter_map(|o| o.span)
            .collect();
        assert_eq!(splice_spans(line, &spans, "text::tokenize"), "use crate::text::tokenize::tokenize;");
    }

    #[test]
    fn subdir_move_produces_decl_create_and_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod tokenize;\npub mod store;\n").unwrap();
        std::fs::write(root.join("src/tokenize.rs"), "pub fn normalize() {}\n").unwrap();
        std::fs::write(root.join("src/store.rs"), "use crate::tokenize::normalize;\npub fn add() { normalize() }\n").unwrap();

        let we = move_workspace_edit(root, "src/tokenize.rs", "src/text/tokenize.rs").expect("edit");
        let s = we.to_string();
        assert!(s.contains("\"kind\":\"create\""), "creates the parent module file: {s}");
        assert!(s.contains("text/mod.rs"), "parent is src/text/mod.rs: {s}");
        assert!(s.contains("pub mod text;"), "old decl repurposed to declare the new top segment: {s}");
        assert!(s.contains("crate::text::tokenize::normalize"), "use path rewritten: {s}");
    }
}
