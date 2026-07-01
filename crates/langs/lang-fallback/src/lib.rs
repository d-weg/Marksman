//! lang-fallback — a tree-sitter [`LanguageProvider`] for languages that don't yet have a
//! SCIP/LSP integration (Python today; Go/Ruby next). It delivers the read path —
//! `structure()` (functions / classes / methods + fn sub-nodes), `import_graph()` (import
//! resolution), and skeletal `outline()` — entirely in-process, plus **UNGATED** structural
//! edits.
//!
//! The honest tradeoff: there is no type-check engine here, so edits are applied through the
//! same VFS/blast-radius machinery as the gated providers but with a no-op gate — they are
//! *structural, not verified*. Callers surface this as `gated: false`. Per language, upgrade
//! to the gated [`GateEngine`] path as its LSP/indexer lands (the Rust provider is the model).
use ci_core::{
    CommitResult, Diag, EditOp, EditOpts, Error, Granularity, ImportGraph, LanguageProvider, Node,
    NodeKind, Result, SymbolKind,
};
use ci_edit::GateEngine;
use ci_treesitter::{syntax_node, ts_range};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tree_sitter::{Node as TsNode, Parser, Point};

/// A language served by the tree-sitter fallback. New languages are a data addition here plus
/// the per-kind mapping in [`collect_items`] — no core changes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FbLang {
    Python,
}

impl FbLang {
    /// Pick a fallback language for `root` by the source files actually present.
    pub fn detect(root: &Path) -> Option<FbLang> {
        if has_ext(root, "py") {
            return Some(FbLang::Python);
        }
        None
    }

    pub fn from_name(name: &str) -> Option<FbLang> {
        match name {
            "python" | "py" => Some(FbLang::Python),
            _ => None,
        }
    }

    fn ts_language(self) -> tree_sitter::Language {
        match self {
            FbLang::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }

    fn ext(self) -> &'static str {
        match self {
            FbLang::Python => "py",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FbLang::Python => "python",
        }
    }
}

#[derive(Clone)]
pub struct FallbackProvider {
    root: PathBuf,
    lang: FbLang,
}

impl FallbackProvider {
    pub fn new(root: &Path, lang: FbLang) -> Self {
        Self { root: root.to_path_buf(), lang }
    }

    /// Fallback edits are never type-checked — there's no compiler/LSP behind them. The MCP
    /// layer reports this as `gated: false` so the agent knows the edit is structural only.
    pub fn gated(&self) -> bool {
        false
    }

    /// Skeletal outline: function/method bodies folded to `...` (valid, idiomatic Python),
    /// signatures and class structure intact. Best-effort: original on a parse failure.
    pub fn outline(&self, content: &str) -> String {
        outline(self.lang, content)
    }

    fn parse(&self, content: &str) -> Option<tree_sitter::Tree> {
        let mut parser = Parser::new();
        parser.set_language(&self.lang.ts_language()).ok()?;
        parser.parse(content, None)
    }

    fn rel(&self, file: &Path) -> String {
        let p = if file.is_absolute() { file.strip_prefix(&self.root).unwrap_or(file) } else { file };
        p.to_string_lossy().replace('\\', "/")
    }
}

impl LanguageProvider for FallbackProvider {
    fn granularity(&self) -> Granularity {
        Granularity::Ast // tree-sitter sub-nodes (params / return / body)
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        let rel = self.rel(file);
        let content = match std::fs::read_to_string(self.root.join(&rel)) {
            Ok(c) => c,
            Err(_) => return Ok(vec![]),
        };
        let Some(tree) = self.parse(&content) else { return Ok(vec![]) };
        let bytes = content.as_bytes();
        let mut out = Vec::new();
        let prefix = format!("{rel}#");
        collect_items(tree.root_node(), bytes, &prefix, SymbolKind::Function, &mut out);
        Ok(out)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        let mut graph: ImportGraph = BTreeMap::new();
        for rel in source_files(&self.root, self.lang.ext()) {
            let Ok(content) = std::fs::read_to_string(self.root.join(&rel)) else { continue };
            let Some(tree) = self.parse(&content) else { continue };
            let mut edges: Vec<PathBuf> = Vec::new();
            collect_imports(tree.root_node(), content.as_bytes(), &rel, &self.root, &mut edges);
            edges.sort();
            edges.dedup();
            if !edges.is_empty() {
                graph.insert(PathBuf::from(&rel), edges);
            }
        }
        Ok(graph)
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        // No type-check engine for this language yet → a no-op gate (always passes). Edits flow
        // through the SAME VFS / blast-radius / atomic-commit path as the gated providers; only
        // the diagnostics step is empty. Structural, not verified — callers report gated: false.
        let mut engine = NoGate::new(&self.root, self.lang);
        let structure_of = |f: &str| self.structure(Path::new(f)).unwrap_or_default();

        // Reverse import map (file -> who imports it) for the delete-safety check.
        let reverse = ci_core::reverse_import_map(&self.import_graph().unwrap_or_default());
        let reverse_imports = |file: &str| reverse.get(file).cloned().unwrap_or_default();

        ci_edit::commit_edits(&self.root, ops, &structure_of, &mut engine, opts, &reverse_imports)
    }
}

/// Skeletal outline for `lang`: fold function/method bodies to `...`, keeping signatures and
/// class structure. Free function so callers (the MCP's extension-keyed dispatch) don't need a
/// provider instance. Best-effort: returns the original on a parse failure.
pub fn outline(lang: FbLang, content: &str) -> String {
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return content.to_string();
    }
    let Some(tree) = parser.parse(content, None) else { return content.to_string() };
    // Fold every `function_definition` body (any body kind); Python uses `...` as the placeholder.
    let bodies = ci_treesitter::body_ranges(tree.root_node(), &["function_definition"], &[]);
    ci_core::elide_bodies_with(content, bodies, "...")
}

// ── the no-op gate (ungated edits) ───────────────────────────────────────────

/// A [`GateEngine`] with no type-checker. `diagnostics` is always empty (the baseline-diff in
/// `commit_edits` then never rejects), so edits are structural-only. `rename` is a best-effort
/// **within-file** textual rename via tree-sitter (every identifier matching the symbol's name
/// in the same file) — honest about its scope, not cross-file like a real LSP. `will_rename`
/// has no importer rewrites to offer.
struct NoGate {
    root: PathBuf,
    lang: FbLang,
}

impl NoGate {
    fn new(root: &Path, lang: FbLang) -> Self {
        Self { root: root.to_path_buf(), lang }
    }

    fn parse(&self, content: &str) -> Option<tree_sitter::Tree> {
        let mut parser = Parser::new();
        parser.set_language(&self.lang.ts_language()).ok()?;
        parser.parse(content, None)
    }
}

impl GateEngine for NoGate {
    fn diagnostics(&mut self, _files: &[(String, String)]) -> Result<Vec<Diag>> {
        Ok(vec![]) // ungated: no type-checker for this language
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
        let ident = if at.kind() == "identifier" { at } else { at.parent().filter(|p| p.kind() == "identifier").unwrap_or(at) };
        let old = ident
            .utf8_text(bytes)
            .map_err(|_| Error::Driver("rename: bad utf8".into()))?
            .to_string();
        if old.is_empty() || ident.kind() != "identifier" {
            return Ok(json!({})); // not a renameable identifier → empty (commit_edits rejects loudly)
        }
        // Every identifier in THIS file with the same text (best-effort, ungated).
        let mut edits = Vec::new();
        collect_identifier_edits(tree.root_node(), bytes, &old, new_name, &mut edits);
        let uri = format!("file://{}", self.root.join(file).to_string_lossy());
        Ok(json!({ "changes": { uri: edits } }))
    }

    fn will_rename(&mut self, _from: &str, _to: &str) -> Result<Value> {
        Ok(json!({})) // no importer-rewrite engine; the move proceeds, blast radius is ungated
    }
}

fn collect_identifier_edits(node: TsNode, bytes: &[u8], old: &str, new: &str, out: &mut Vec<Value>) {
    if node.kind() == "identifier" && node.utf8_text(bytes).map(|t| t == old).unwrap_or(false) {
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

// ── structure ────────────────────────────────────────────────────────────────

/// Walk a statement list, emitting a `Node` per function / class. `fn_kind` is the kind for
/// functions found here (Function at module level, Method inside a class body). `prefix` is the
/// id stem (`"file.py#"`, or `"file.py#Class."` inside a class).
fn collect_items(node: TsNode, bytes: &[u8], prefix: &str, fn_kind: SymbolKind, out: &mut Vec<Node>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" => emit_fn(&child, bytes, prefix, fn_kind, out),
            "class_definition" => emit_class(&child, bytes, prefix, out),
            "decorated_definition" => {
                if let Some(def) = child.child_by_field_name("definition") {
                    match def.kind() {
                        "function_definition" => emit_fn(&def, bytes, prefix, fn_kind, out),
                        "class_definition" => emit_class(&def, bytes, prefix, out),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn emit_fn(def: &TsNode, bytes: &[u8], prefix: &str, fn_kind: SymbolKind, out: &mut Vec<Node>) {
    if let Some(mut n) = named_node(def, bytes, prefix, fn_kind) {
        add_fn_subnodes(&mut n, def, bytes);
        out.push(n);
    }
}

fn emit_class(def: &TsNode, bytes: &[u8], prefix: &str, out: &mut Vec<Node>) {
    if let Some(mut n) = named_node(def, bytes, prefix, SymbolKind::Class) {
        let inner = format!("{prefix}{}.", n.name.as_deref().unwrap_or_default());
        if let Some(body) = def.child_by_field_name("body") {
            // class docstring → `:doc` anchor (parity with functions/methods).
            if let Some(ds) = python_docstring(&body) {
                n.children.push(syntax_node(&format!("{}:doc", n.id), None, "doc", &ds));
            }
        }
        out.push(n);
        if let Some(body) = def.child_by_field_name("body") {
            collect_items(body, bytes, &inner, SymbolKind::Method, out);
        }
    }
}

fn named_node(item: &TsNode, bytes: &[u8], prefix: &str, kind: SymbolKind) -> Option<Node> {
    let name_node = item.child_by_field_name("name")?;
    let name = name_node.utf8_text(bytes).ok()?.to_string();
    Some(Node {
        id: format!("{prefix}{name}"),
        name: Some(name),
        kind: NodeKind::Symbol(kind),
        range: ts_range(item),
        name_range: Some(ts_range(&name_node)),
        children: vec![],
    })
}

fn add_fn_subnodes(n: &mut Node, item: &TsNode, bytes: &[u8]) {
    if let Some(params) = item.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for (i, p) in params.named_children(&mut cursor).enumerate() {
            // skip `self`/`cls` — they aren't meaningful edit targets
            let name = p.utf8_text(bytes).ok().map(str::to_string);
            if matches!(name.as_deref(), Some("self") | Some("cls")) {
                continue;
            }
            n.children.push(syntax_node(&format!("{}:param.{i}", n.id), name, "parameter", &p));
        }
    }
    if let Some(rt) = item.child_by_field_name("return_type") {
        n.children.push(syntax_node(&format!("{}:return", n.id), None, "returnType", &rt));
    }
    if let Some(body) = item.child_by_field_name("body") {
        // Docstring = the first statement when it's a bare string literal — the `:doc` anchor.
        if let Some(ds) = python_docstring(&body) {
            n.children.push(syntax_node(&format!("{}:doc", n.id), None, "doc", &ds));
        }
        n.children.push(syntax_node(&format!("{}:body", n.id), None, "body", &body));
    }
}

/// The docstring node of a function/class body: its first statement, if that statement is a bare
/// string expression (`"""…"""` / `'…'`).
fn python_docstring<'a>(body: &TsNode<'a>) -> Option<TsNode<'a>> {
    let first = body.named_child(0)?;
    if first.kind() == "expression_statement" {
        let s = first.named_child(0)?;
        if s.kind() == "string" {
            return Some(s);
        }
    }
    None
}

// ── import graph ─────────────────────────────────────────────────────────────

/// Collect resolvable import edges from a file's syntax tree.
fn collect_imports(node: TsNode, bytes: &[u8], from_rel: &str, root: &Path, out: &mut Vec<PathBuf>) {
    match node.kind() {
        "import_statement" => {
            // `import a.b.c [as x], d.e` — each dotted name is an absolute module.
            let mut c = node.walk();
            for ch in node.named_children(&mut c) {
                let dotted = match ch.kind() {
                    "dotted_name" => Some(ch),
                    "aliased_import" => ch.child_by_field_name("name"),
                    _ => None,
                };
                if let Some(d) = dotted {
                    let parts = dotted_parts(&d, bytes);
                    push_absolute(root, &parts, out);
                }
            }
        }
        "import_from_statement" => {
            if let Some(module) = node.child_by_field_name("module_name") {
                let (level, mod_parts) = module_spec(&module, bytes);
                if let Some(base) = base_dir(from_rel, level) {
                    // the module itself (`from a.b import …` → a/b.py or a/b/__init__.py)
                    if !mod_parts.is_empty() {
                        if let Some(p) = resolve(root, &base, &mod_parts) {
                            out.push(p);
                        }
                    }
                    // each imported name, in case it's a submodule (`from pkg import sub`)
                    for name in imported_names(&node, &module, bytes) {
                        let mut parts = mod_parts.clone();
                        parts.push(name);
                        if let Some(p) = resolve(root, &base, &parts) {
                            out.push(p);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    // Recurse: imports can be nested (inside functions / try blocks).
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_imports(ch, bytes, from_rel, root, out);
    }
}

/// `import a.b.c` → try `a/b/c.py`, `a/b/c/__init__.py` from the repo root and `src/`.
fn push_absolute(root: &Path, parts: &[String], out: &mut Vec<PathBuf>) {
    for base in [PathBuf::new(), PathBuf::from("src")] {
        if let Some(p) = resolve(root, &base, parts) {
            out.push(p);
            return;
        }
    }
}

/// Split a `dotted_name` into its identifier parts.
fn dotted_parts(node: &TsNode, bytes: &[u8]) -> Vec<String> {
    node.utf8_text(bytes).unwrap_or("").split('.').filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// `(level, parts)` for a `module_name`: a `dotted_name` is absolute (level 0); a
/// `relative_import` carries leading dots (level = dot count) and an optional dotted tail.
fn module_spec(node: &TsNode, bytes: &[u8]) -> (usize, Vec<String>) {
    match node.kind() {
        "dotted_name" => (0, dotted_parts(node, bytes)),
        "relative_import" => {
            let mut level = 0;
            let mut parts = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                match ch.kind() {
                    "import_prefix" => level = ch.utf8_text(bytes).unwrap_or("").matches('.').count(),
                    "dotted_name" => parts = dotted_parts(&ch, bytes),
                    _ => {}
                }
            }
            (level.max(1), parts)
        }
        _ => (0, vec![]),
    }
}

/// The imported names of a `from … import a, b as c` (skips the module_name node + wildcard).
fn imported_names(stmt: &TsNode, module: &TsNode, bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut c = stmt.walk();
    for ch in stmt.named_children(&mut c) {
        if ch.id() == module.id() {
            continue;
        }
        match ch.kind() {
            "dotted_name" => {
                if let Some(first) = dotted_parts(&ch, bytes).into_iter().next() {
                    names.push(first);
                }
            }
            "aliased_import" => {
                if let Some(n) = ch.child_by_field_name("name") {
                    if let Some(first) = dotted_parts(&n, bytes).into_iter().next() {
                        names.push(first);
                    }
                }
            }
            _ => {}
        }
    }
    names
}

/// The package directory a relative import is anchored at. Level 0 (absolute) → repo root;
/// level 1 → the file's own directory; each extra dot ascends one more.
fn base_dir(from_rel: &str, level: usize) -> Option<PathBuf> {
    if level == 0 {
        return Some(PathBuf::new());
    }
    let mut dir = Path::new(from_rel).parent()?.to_path_buf();
    for _ in 1..level {
        dir = dir.parent()?.to_path_buf();
    }
    Some(dir)
}

/// Resolve `base/parts…` to a repo-relative `.py` file or package `__init__.py`, if it exists.
fn resolve(root: &Path, base: &Path, parts: &[String]) -> Option<PathBuf> {
    if parts.is_empty() {
        return None;
    }
    let mut p = base.to_path_buf();
    for part in parts {
        p.push(part);
    }
    let as_file = p.with_extension("py");
    if root.join(&as_file).is_file() {
        return Some(norm(&as_file));
    }
    let as_init = p.join("__init__.py");
    if root.join(&as_init).is_file() {
        return Some(norm(&as_init));
    }
    None
}

fn norm(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().replace('\\', "/"))
}

/// Repo-relative source files with the given extension, gitignore-aware.
fn source_files(root: &Path, ext: &str) -> Vec<String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out
}

fn has_ext(root: &Path, ext: &str) -> bool {
    !source_files(root, ext).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn py_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/__init__.py"), "").unwrap();
        fs::write(
            root.join("pkg/math_utils.py"),
            "def add(a, b):\n    return a + b\n\n\nclass Calc:\n    def total(self, xs) -> int:\n        return sum(xs)\n",
        )
        .unwrap();
        fs::write(
            root.join("app.py"),
            "from pkg.math_utils import add, Calc\nimport pkg\n\n\ndef main():\n    return add(1, 2)\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn structure_extracts_functions_classes_methods() {
        let dir = py_project();
        let p = FallbackProvider::new(dir.path(), FbLang::Python);
        let nodes = p.structure(Path::new("pkg/math_utils.py")).unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"pkg/math_utils.py#add"), "module fn: {ids:?}");
        assert!(ids.contains(&"pkg/math_utils.py#Calc"), "class: {ids:?}");
        assert!(ids.contains(&"pkg/math_utils.py#Calc.total"), "method qualified by class: {ids:?}");
        // the method carries return + body sub-nodes (and `self` is dropped from params)
        let total = nodes.iter().find(|n| n.id == "pkg/math_utils.py#Calc.total").unwrap();
        let kinds: Vec<&str> = total
            .children
            .iter()
            .filter_map(|c| match &c.kind {
                NodeKind::Syntax(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&"body") && kinds.contains(&"returnType"), "sub-nodes: {kinds:?}");
        assert!(total.children.iter().all(|c| c.name.as_deref() != Some("self")), "self dropped");
    }

    #[test]
    fn python_docstring_becomes_doc_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("m.py"), "def f(x):\n    \"\"\"Return x.\"\"\"\n    return x\n").unwrap();
        let p = FallbackProvider::new(root, FbLang::Python);
        let nodes = p.structure(Path::new("m.py")).unwrap();
        let f = nodes.iter().find(|n| n.id == "m.py#f").unwrap();
        assert!(f.children.iter().any(|c| c.id == "m.py#f:doc"), "doc anchor: {:?}", f.children);
    }

    #[test]
    fn import_graph_resolves_from_and_import() {
        let dir = py_project();
        let p = FallbackProvider::new(dir.path(), FbLang::Python);
        let g = p.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("app.py")).expect("app.py edges");
        assert!(edges.contains(&PathBuf::from("pkg/math_utils.py")), "from pkg.math_utils: {edges:?}");
        assert!(edges.contains(&PathBuf::from("pkg/__init__.py")), "import pkg: {edges:?}");
    }

    #[test]
    fn outline_folds_bodies() {
        let dir = py_project();
        let p = FallbackProvider::new(dir.path(), FbLang::Python);
        let src = fs::read_to_string(dir.path().join("pkg/math_utils.py")).unwrap();
        let out = p.outline(&src);
        assert!(out.contains("def add(a, b):"), "signature kept: {out}");
        assert!(out.contains("..."), "body folded: {out}");
        assert!(!out.contains("return a + b"), "body elided: {out}");
        assert!(out.contains("class Calc:"), "class structure kept: {out}");
    }

    #[test]
    fn ungated_replace_node_commits() {
        let dir = py_project();
        let root = dir.path();
        let p = FallbackProvider::new(root, FbLang::Python);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let res = p
            .apply_edits(
                &[EditOp::ReplaceNode {
                    node_id: "pkg/math_utils.py#add".into(),
                    code: "def add(a, b):\n    s = a + b\n    return s".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "ungated replace must commit: {res:?}");
        assert!(fs::read_to_string(root.join("pkg/math_utils.py")).unwrap().contains("s = a + b"));
    }

    #[test]
    fn ungated_set_body_replaces_block() {
        let dir = py_project();
        let root = dir.path();
        let p = FallbackProvider::new(root, FbLang::Python);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        // set_body narrows to the `:body` anchor (the suite) — signature stays.
        let res = p
            .apply_edits(
                &[EditOp::SetBody { node_id: "pkg/math_utils.py#add".into(), body: "return a - b".into() }],
                &opts,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "ungated set_body must commit: {res:?}");
        let after = fs::read_to_string(root.join("pkg/math_utils.py")).unwrap();
        assert!(after.contains("def add(a, b):"), "signature intact: {after}");
        assert!(after.contains("return a - b") && !after.contains("return a + b"), "body replaced: {after}");
    }

    #[test]
    fn ungated_rename_rewrites_within_file() {
        let dir = py_project();
        let root = dir.path();
        let p = FallbackProvider::new(root, FbLang::Python);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let res = p
            .apply_edits(
                &[EditOp::Rename { node_id: "pkg/math_utils.py#add".into(), new_name: "plus".into() }],
                &opts,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "rename should commit: {res:?}");
        let after = fs::read_to_string(root.join("pkg/math_utils.py")).unwrap();
        assert!(after.contains("def plus(a, b):"), "definition renamed: {after}");
    }
}
