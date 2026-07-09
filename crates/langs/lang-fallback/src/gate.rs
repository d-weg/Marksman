//! The no-op gate behind UNGATED edits: a [`GateEngine`] whose only verdict is tree-sitter's
//! parse (new ERROR/missing nodes reject, pre-existing breakage never blocks — a syntax gate,
//! not a compiler), plus best-effort within-file rename and syntactic JS/TS import-specifier
//! rewriting on file moves.
use ci_core::{Diag, Error, Result};
use ci_edit::GateEngine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tree_sitter::{Node as TsNode, Parser, Point};

use crate::imports::{resolve_js_specifier, source_files};
use crate::FbLang;

/// A [`GateEngine`] with no type-checker. `diagnostics` is always empty (the baseline-diff in
/// `commit_edits` then never rejects), so edits are structural-only. `rename` is a best-effort
/// **within-file** textual rename via tree-sitter (every identifier matching the symbol's name
/// in the same file) — honest about its scope, not cross-file like a real LSP. `will_rename`
/// has no importer rewrites to offer.
pub(crate) struct NoGate {
    root: PathBuf,
    lang: FbLang,
}

impl NoGate {
    pub(crate) fn new(root: &Path, lang: FbLang) -> Self {
        Self { root: root.to_path_buf(), lang }
    }

    fn parse(&self, content: &str) -> Option<tree_sitter::Tree> {
        let mut parser = Parser::new();
        parser.set_language(&self.lang.ts_language()).ok()?;
        parser.parse(content, None)
    }
}

impl GateEngine for NoGate {
    /// tree-sitter can't type-check, but it CAN parse — and since it never refuses input
    /// (error-RECOVERING: any bytes yield a tree, breakage becomes ERROR/missing nodes), the
    /// way to "ask" it whether the edited content is acceptable is to count those nodes.
    /// `commit_edits`' baseline-diff then rejects any edit introducing a NEW syntax error
    /// (the unbalanced brace a bad set_body leaves behind) while pre-existing breakage never
    /// blocks an unrelated edit. Honest limit: a syntax gate, not a compiler — some invalid
    /// code still parses clean.
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        let mut out = Vec::new();
        for (path, content) in files {
            let Some(tree) = self.parse(content) else { continue };
            collect_syntax_errors(tree.root_node(), content, path, &mut out);
        }
        Ok(out)
    }

    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<Value> {
        let content = std::fs::read_to_string(self.root.join(file))
            .map_err(|e| Error::Driver(format!("rename: reading {file}: {e}")))?;
        let bytes = content.as_bytes();
        let tree = self.parse(&content).ok_or_else(|| Error::Driver("rename: parse failed".into()))?;
        let pt = Point { row: line as usize, column: character as usize };
        let at = tree
            .root_node()
            .named_descendant_for_point_range(pt, pt)
            .ok_or_else(|| Error::Driver("rename: no node at position".into()))?;
        // Walk out to the enclosing identifier if the point landed on a child token.
        let ident = if is_ident(&at) { at } else { at.parent().filter(is_ident).unwrap_or(at) };
        let old = ident
            .utf8_text(bytes)
            .map_err(|_| Error::Driver("rename: bad utf8".into()))?
            .to_string();
        if old.is_empty() || !is_ident(&ident) {
            return Ok(json!({})); // not a renameable identifier → empty (commit_edits rejects loudly)
        }
        // Every identifier in THIS file with the same text (best-effort, ungated).
        let mut edits = Vec::new();
        collect_identifier_edits(tree.root_node(), bytes, &old, new_name, &mut edits);
        let uri = format!("file://{}", self.root.join(file).to_string_lossy());
        Ok(json!({ "changes": { uri: edits } }))
    }

    /// JS/TS moves rewrite importers SYNTACTICALLY — the same job the compiler does in gated
    /// mode, minus type knowledge: every relative specifier that resolves to `from` retargets
    /// to `to`, and the MOVED file's own relative specifiers are recomputed from its new
    /// directory. Quote and extension style are preserved (`"./x.js"` stays a `.js` specifier
    /// even though the file on disk is `.ts`). Other fallback languages import by
    /// package/module name, not file path — nothing to rewrite, the move proceeds as before.
    fn will_rename(&mut self, from: &str, to: &str) -> Result<Value> {
        if !matches!(self.lang, FbLang::Js | FbLang::Ts) {
            return Ok(json!({}));
        }
        let to_dir = Path::new(to).parent().unwrap_or(Path::new("")).to_path_buf();
        let mut changes = serde_json::Map::new();
        for ext in self.lang.exts() {
            for rel in source_files(&self.root, ext) {
                let Ok(content) = std::fs::read_to_string(self.root.join(&rel)) else { continue };
                let Some(tree) = self.parse(&content) else { continue };
                let mut specs = Vec::new();
                collect_spec_nodes(tree.root_node(), content.as_bytes(), &mut specs);
                let mut edits = Vec::new();
                for (range, spec) in specs {
                    if !(spec.starts_with("./") || spec.starts_with("../")) {
                        continue;
                    }
                    let new_spec = if rel == from {
                        // The moved file's own imports: whatever they resolve to today, the
                        // path there is different from the NEW directory.
                        resolve_js_specifier(&self.root, &rel, &spec)
                            .map(|target| with_spec_ext(relative_specifier(&to_dir, &target), &spec))
                    } else if resolve_js_specifier(&self.root, &rel, &spec).as_deref() == Some(Path::new(from)) {
                        // An importer of the moved file: retarget to the new location.
                        let rel_dir = Path::new(&rel).parent().unwrap_or(Path::new("")).to_path_buf();
                        Some(with_spec_ext(relative_specifier(&rel_dir, Path::new(to)), &spec))
                    } else {
                        None
                    };
                    if let Some(ns) = new_spec {
                        if ns != spec {
                            edits.push(json!({ "range": range, "newText": ns }));
                        }
                    }
                }
                if !edits.is_empty() {
                    let uri = format!("file://{}", self.root.join(&rel).to_string_lossy());
                    changes.insert(uri, Value::Array(edits));
                }
            }
        }
        Ok(json!({ "changes": changes }))
    }
}

/// Identifier-ish token across grammars: `identifier`, `field_identifier`, `type_identifier`
/// (go/c/cpp/java), ruby's `constant`.
fn is_ident(n: &TsNode) -> bool {
    n.kind().ends_with("identifier") || n.kind() == "constant"
}

/// ERROR / missing nodes → `Diag`s. The message embeds a short source excerpt rather than the
/// line number, so a pre-existing error whose line SHIFTS under an edit keeps an identical
/// message — baseline-diff keys on (file, code, message) and must not re-flag it as new.
/// ERROR subtrees are not descended (nested noise); capped per file.
fn collect_syntax_errors(root: TsNode, content: &str, file: &str, out: &mut Vec<Diag>) {
    let mut stack = vec![root];
    let mut count = 0;
    while let Some(n) = stack.pop() {
        if count >= 10 {
            return;
        }
        if n.is_error() || n.is_missing() {
            let line = n.start_position().row as u32 + 1;
            let message = if n.is_missing() {
                format!("syntax error: missing `{}`", n.kind())
            } else {
                let excerpt: String = content[n.byte_range()].chars().take(40).collect();
                format!("syntax error near `{}`", excerpt.trim())
            };
            out.push(Diag { file: file.to_string(), code: 0, message, line });
            count += 1;
            continue; // don't descend into an ERROR subtree
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Import/export specifier STRING nodes: `(inner range excluding quotes, specifier text)`.
/// Specifiers never span lines, so the inner range is start col+1 .. end col-1.
fn collect_spec_nodes(node: TsNode, bytes: &[u8], out: &mut Vec<(Value, String)>) {
    if matches!(node.kind(), "import_statement" | "export_statement") {
        if let Some(src) = node.child_by_field_name("source") {
            let raw = src.utf8_text(bytes).unwrap_or("");
            let spec = raw.trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string();
            let (s, e) = (src.start_position(), src.end_position());
            let range = json!({
                "start": { "line": s.row, "character": s.column + 1 },
                "end": { "line": e.row, "character": e.column.saturating_sub(1) },
            });
            out.push((range, spec));
        }
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_spec_nodes(ch, bytes, out);
    }
}

/// `./`-style relative path from `from_dir` to `target` (both repo-relative).
fn relative_specifier(from_dir: &Path, target: &Path) -> String {
    let f: Vec<_> = from_dir.components().collect();
    let t: Vec<_> = target.components().collect();
    let common = f.iter().zip(t.iter()).take_while(|(a, b)| a == b).count();
    let mut s = String::new();
    if f.len() == common {
        s.push_str("./");
    } else {
        for _ in common..f.len() {
            s.push_str("../");
        }
    }
    let tail: Vec<String> = t[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    s + &tail.join("/")
}

/// Restyle `path_spec`'s extension to match how `old_spec` wrote it: `"./x.js"` keeps `.js`
/// (TS convention), an extension-less specifier stays extension-less.
fn with_spec_ext(path_spec: String, old_spec: &str) -> String {
    let mut base = path_spec;
    let slash = base.rfind('/').map_or(0, |i| i + 1);
    if let Some(dot) = base[slash..].rfind('.') {
        base.truncate(slash + dot);
    }
    let old_leaf = old_spec.rsplit('/').next().unwrap_or(old_spec);
    if let Some(dot) = old_leaf.rfind('.') {
        base.push_str(&old_leaf[dot..]);
    }
    base
}

fn collect_identifier_edits(node: TsNode, bytes: &[u8], old: &str, new: &str, out: &mut Vec<Value>) {
    if is_ident(&node) && node.utf8_text(bytes).map(|t| t == old).unwrap_or(false) {
        let s = node.start_position();
        let e = node.end_position();
        out.push(json!({
            "range": {
                "start": { "line": s.row, "character": s.column },
                "end": { "line": e.row, "character": e.column },
            },
            "newText": new,
        }));
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_identifier_edits(ch, bytes, old, new, out);
    }
}
