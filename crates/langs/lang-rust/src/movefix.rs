//! movefix — the syntactic module-move rewrite rust-analyzer doesn't provide.
//!
//! ra's `willRenameFiles` rewrites same-directory renames but returns NOTHING for a move
//! into a submodule (`src/a.rs` → `src/d/a.rs`), leaving the `mod` declaration and every
//! `crate::a::…` path dangling — the agent then has to finish the move by hand (bench
//! `move-rust`: +10 turns of manual edits). This module computes those rewrites as a genuine
//! LSP `WorkspaceEdit` (documentChanges: CreateFile + TextEdits), applied through the same
//! `apply_workspace_edit` path a server's edits would take.
//!
//! Best-effort BY DESIGN: it handles crate-root moves/renames of leaf modules (the common
//! agent operations); anything it misses is caught by the type-check gate, which rejects
//! with named sites — the fallback can be incomplete, it can never be silently wrong.
use serde_json::{json, Value};
use std::path::Path;

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

/// Find the `[pub] mod <name>;` declaration line in `content`. Returns (line index, line,
/// visibility prefix e.g. `"pub "`).
fn find_mod_decl(content: &str, name: &str) -> Option<(usize, String, String)> {
    for (i, line) in content.lines().enumerate() {
        let t = line.trim_start();
        let (vis, rest) = match t.strip_prefix("pub ") {
            Some(r) => ("pub ", r.trim_start()),
            None => ("", t),
        };
        if let Some(decl) = rest.strip_prefix("mod ") {
            if decl.trim_end().strip_suffix(';').map(str::trim) == Some(name) {
                return Some((i, line.to_string(), vis.to_string()));
            }
        }
    }
    None
}

/// Rewrite every `crate::<old_path>` to `crate::<new_path>` in `line` (token-bounded), plus
/// bare `<old_head>::` heads when `bare_head` is set (references in the old declaring file,
/// where the module was in scope unqualified).
fn rewrite_line(line: &str, old_path: &str, new_path: &str, bare: Option<(&str, &str)>) -> String {
    let mut out = line.replace(&format!("crate::{old_path}"), &format!("crate::{new_path}"));
    if let Some((old_head, new_head)) = bare {
        // token-bounded bare rewrite: `tokenize::x` → `text::tokenize::x`, but never inside
        // a longer identifier and never when already crate-qualified (handled above).
        let needle = format!("{old_head}::");
        let mut res = String::new();
        let mut rest = out.as_str();
        while let Some(pos) = rest.find(&needle) {
            let before_ok = pos == 0
                || !rest[..pos].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':');
            res.push_str(&rest[..pos]);
            if before_ok {
                res.push_str(new_head);
                res.push_str("::");
            } else {
                res.push_str(&needle);
            }
            rest = &rest[pos + needle.len()..];
        }
        res.push_str(rest);
        out = res;
    }
    out
}

/// The move's `WorkspaceEdit` (LSP `documentChanges`: CreateFile + TextEdits), or `None` when
/// the move shape is outside what this fallback understands (the gate still protects the
/// commit either way).
pub(crate) fn move_workspace_edit(root: &Path, from: &str, to: &str) -> Option<Value> {
    let from_segs = mod_segs(from)?;
    let to_segs = mod_segs(to)?;
    let old_name = from_segs.last()?.clone();
    let new_name = to_segs.last()?.clone();
    let old_path = from_segs.join("::");
    let new_path = to_segs.join("::");
    if old_path == new_path {
        return None;
    }

    let uri = |rel: &str| format!("file://{}/{}", root.to_string_lossy(), rel);
    let mut doc_changes: Vec<Value> = Vec::new();

    // 1. The OLD declaring file: drop (or repurpose) the `mod <old>;` line.
    let (old_parent, old_parent_missing) = parent_decl_file(root, &from_segs);
    if old_parent_missing {
        return None; // pathological: the moved module was never declared
    }
    let old_parent_content = std::fs::read_to_string(root.join(&old_parent)).ok()?;
    let (decl_idx, decl_line, vis) = find_mod_decl(&old_parent_content, &old_name)?;

    let (new_parent, new_parent_missing) = parent_decl_file(root, &to_segs);
    let same_parent = new_parent == old_parent;

    let mut old_parent_edits: Vec<Value> = Vec::new();
    if same_parent {
        // rename in place: `mod a;` → `mod b;`
        old_parent_edits.push(line_edit(decl_idx, &decl_line, &format!("{vis}mod {new_name};"), false));
    } else {
        // Supported shape: a TOP-LEVEL module moving one level down (src/a.rs →
        // src/d/a.rs). Deeper/lateral moves return None — the agent's manual path, still
        // gate-protected. The new top segment is declared where the old module was:
        // repurpose the old decl line when `d` isn't declared yet, else just delete it.
        if from_segs.len() != 1 || to_segs.len() != 2 {
            return None;
        }
        let new_top = &to_segs[0];
        if find_mod_decl(&old_parent_content, new_top).is_some() {
            old_parent_edits.push(line_edit(decl_idx, &decl_line, "", true));
        } else {
            old_parent_edits.push(line_edit(decl_idx, &decl_line, &format!("{vis}mod {new_top};"), false));
        }
    }

    // 2. The NEW parent module file declares the moved module (skip when renaming in place).
    if !same_parent {
        if new_parent_missing {
            doc_changes.push(json!({"kind": "create", "uri": uri(&new_parent)}));
            doc_changes.push(json!({
                "textDocument": {"uri": uri(&new_parent), "version": null},
                "edits": [ {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                            "newText": format!("pub mod {new_name};\n")} ],
            }));
        } else {
            let content = std::fs::read_to_string(root.join(&new_parent)).ok()?;
            if find_mod_decl(&content, &new_name).is_none() {
                let lines = content.lines().count();
                doc_changes.push(json!({
                    "textDocument": {"uri": uri(&new_parent), "version": null},
                    "edits": [ {"range": {"start": {"line": lines, "character": 0}, "end": {"line": lines, "character": 0}},
                                "newText": format!("pub mod {new_name};\n")} ],
                }));
            }
        }
    }

    // 3. Path rewrites: `crate::<old>` → `crate::<new>` everywhere; bare `<old>::` heads in
    //    the old declaring file (there the module was in scope unqualified).
    for rel in super::rust_files(root) {
        if rel == from {
            continue; // the moved file's own content travels as-is
        }
        let content = std::fs::read_to_string(root.join(&rel)).ok()?;
        let bare = (rel == old_parent).then(|| (old_name.as_str(), new_path.as_str()));
        let mut edits: Vec<Value> = if rel == old_parent { old_parent_edits.clone() } else { Vec::new() };
        for (i, line) in content.lines().enumerate() {
            if rel == old_parent && i == decl_idx {
                continue; // already covered by the decl edit
            }
            let rewritten = rewrite_line(line, &old_path, &new_path, bare);
            if rewritten != line {
                edits.push(line_edit(i, line, &rewritten, false));
            }
        }
        if !edits.is_empty() {
            doc_changes.push(json!({"textDocument": {"uri": uri(&rel), "version": null}, "edits": edits}));
        }
    }

    Some(json!({"documentChanges": doc_changes}))
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

    #[test]
    fn bare_rewrite_is_token_bounded() {
        assert_eq!(
            rewrite_line("    util::one() + myutil::two()", "util", "core::util", Some(("util", "core::util"))),
            "    core::util::one() + myutil::two()"
        );
        assert_eq!(
            rewrite_line("use crate::tokenize::tokenize;", "tokenize", "text::tokenize", None),
            "use crate::text::tokenize::tokenize;"
        );
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
