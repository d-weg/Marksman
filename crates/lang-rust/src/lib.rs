//! lang-rust — the Rust [`LanguageProvider`]. v0 read path: in-process `tree-sitter-rust`
//! (no external tooling — Rust's parser is a Rust crate) for `structure()` (items + fn
//! sub-nodes) and `import_graph()` (`mod` resolution). Compiler-accurate references and
//! type-checked edits via rust-analyzer are on the roadmap; this is what lets CodeGraph
//! index and retrieve Rust — including its own source — today.
use ci_core::{
    CommitResult, EditOp, EditOpts, Error, Granularity, ImportGraph, LanguageProvider, Node,
    NodeKind, Range, Result, SymbolKind,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tree_sitter::{Node as TsNode, Parser};

pub struct RustProvider {
    root: PathBuf,
}

impl RustProvider {
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf() }
    }

    /// Normalize a (possibly absolute) path to the repo-relative posix form.
    fn rel(&self, file: &Path) -> String {
        let p = if file.is_absolute() { file.strip_prefix(&self.root).unwrap_or(file) } else { file };
        p.to_string_lossy().replace('\\', "/")
    }

    fn parse(content: &str) -> Option<tree_sitter::Tree> {
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).ok()?;
        parser.parse(content, None)
    }
}

impl LanguageProvider for RustProvider {
    fn granularity(&self) -> Granularity {
        Granularity::Ast // tree-sitter sub-nodes (params / return / body)
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        let rel = self.rel(file);
        let content = match std::fs::read_to_string(self.root.join(&rel)) {
            Ok(c) => c,
            Err(_) => return Ok(vec![]),
        };
        let Some(tree) = Self::parse(&content) else { return Ok(vec![]) };
        let bytes = content.as_bytes();
        let mut out = Vec::new();
        let prefix = format!("{rel}#");
        collect_items(tree.root_node(), bytes, &prefix, SymbolKind::Function, &mut out);
        Ok(out)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        let mut graph: ImportGraph = BTreeMap::new();
        for rel in rust_files(&self.root) {
            let abs = self.root.join(&rel);
            let Ok(content) = std::fs::read_to_string(&abs) else { continue };
            let Some(tree) = Self::parse(&content) else { continue };
            let mut edges = Vec::new();
            for module in mod_decls(tree.root_node(), content.as_bytes()) {
                if let Some(target) = resolve_mod(&self.root, &rel, &module) {
                    edges.push(target);
                }
            }
            if !edges.is_empty() {
                graph.insert(PathBuf::from(&rel), edges);
            }
        }
        Ok(graph)
    }

    fn apply_edits(&self, _ops: &[EditOp], _opts: &EditOpts) -> Result<CommitResult> {
        // v0 is read-only. Type-checked Rust edits (rust-analyzer GateEngine) are on the roadmap.
        Err(Error::Driver(
            "Rust edits are not supported yet (read-only provider); the rust-analyzer edit gate is on the roadmap".into(),
        ))
    }
}

// ── structure ──────────────────────────────────────────────────────────────

/// Walk an item list, emitting a `Node` per named declaration. `fn_kind` is the kind for
/// `function_item`s found here (Function at top level, Method inside an `impl`). `prefix` is
/// the id stem (`"file.rs#"`, or `"file.rs#Type."` inside an impl).
fn collect_items(node: TsNode, bytes: &[u8], prefix: &str, fn_kind: SymbolKind, out: &mut Vec<Node>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(mut n) = named_node(&child, bytes, prefix, fn_kind) {
                    add_fn_subnodes(&mut n, &child, bytes);
                    out.push(n);
                }
            }
            "struct_item" | "union_item" => push(&child, bytes, prefix, SymbolKind::Struct, out),
            "enum_item" => push(&child, bytes, prefix, SymbolKind::Enum, out),
            "trait_item" => push(&child, bytes, prefix, SymbolKind::Interface, out),
            "type_item" => push(&child, bytes, prefix, SymbolKind::TypeAlias, out),
            "const_item" | "static_item" => push(&child, bytes, prefix, SymbolKind::Variable, out),
            "macro_definition" => push(&child, bytes, prefix, SymbolKind::Function, out),
            "impl_item" => {
                let ty = child
                    .child_by_field_name("type")
                    .and_then(|t| type_text(&t, bytes))
                    .unwrap_or_else(|| "impl".to_string());
                if let Some(body) = child.child_by_field_name("body") {
                    let inner = format!("{prefix}{ty}.");
                    collect_items(body, bytes, &inner, SymbolKind::Method, out);
                }
            }
            "mod_item" => {
                if let Some(body) = child.child_by_field_name("body") {
                    collect_items(body, bytes, prefix, SymbolKind::Function, out);
                }
            }
            _ => {}
        }
    }
}

fn push(item: &TsNode, bytes: &[u8], prefix: &str, kind: SymbolKind, out: &mut Vec<Node>) {
    if let Some(n) = named_node(item, bytes, prefix, kind) {
        out.push(n);
    }
}

/// Build a declaration `Node` from an item with a `name` field.
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

/// Attach params / return type / body as `Syntax` sub-nodes of a function/method.
fn add_fn_subnodes(n: &mut Node, item: &TsNode, bytes: &[u8]) {
    if let Some(params) = item.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for (i, p) in params.named_children(&mut cursor).enumerate() {
            let name = p.utf8_text(bytes).ok().map(str::to_string);
            n.children.push(syntax(&format!("{}:param.{i}", n.id), name, "parameter", &p));
        }
    }
    if let Some(rt) = item.child_by_field_name("return_type") {
        n.children.push(syntax(&format!("{}:return", n.id), None, "returnType", &rt));
    }
    if let Some(body) = item.child_by_field_name("body") {
        n.children.push(syntax(&format!("{}:body", n.id), None, "body", &body));
    }
}

fn syntax(id: &str, name: Option<String>, kind: &str, n: &TsNode) -> Node {
    Node {
        id: id.to_string(),
        name,
        kind: NodeKind::Syntax(kind.to_string()),
        range: ts_range(n),
        name_range: None,
        children: vec![],
    }
}

/// First `type_identifier` inside an impl's `type` node (the base type being implemented).
fn type_text(t: &TsNode, bytes: &[u8]) -> Option<String> {
    if t.kind() == "type_identifier" {
        return t.utf8_text(bytes).ok().map(str::to_string);
    }
    let mut cursor = t.walk();
    for c in t.named_children(&mut cursor) {
        if c.kind() == "type_identifier" {
            return c.utf8_text(bytes).ok().map(str::to_string);
        }
    }
    t.utf8_text(bytes).ok().map(str::to_string)
}

fn ts_range(n: &TsNode) -> Range {
    let s = n.start_position();
    let e = n.end_position();
    Range {
        start_line: s.row as u32 + 1,
        start_char: s.column as u32,
        end_line: e.row as u32 + 1,
        end_char: e.column as u32,
    }
}

// ── import graph (mod resolution) ────────────────────────────────────────────

/// `mod foo;` declarations (file modules only — inline `mod foo { … }` has no file edge).
fn mod_decls(root: TsNode, bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "mod_item" && child.child_by_field_name("body").is_none() {
            if let Some(name) = child.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok()) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Resolve `mod <module>;` declared in `from` (repo-relative) to a repo-relative file.
/// A directory module (`mod.rs`, `lib.rs`, `main.rs`) resolves submodules in its own dir;
/// a file module `foo.rs` resolves them under `foo/`.
fn resolve_mod(root: &Path, from: &str, module: &str) -> Option<PathBuf> {
    let from_path = Path::new(from);
    let parent = from_path.parent().unwrap_or(Path::new(""));
    let stem = from_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let base = if matches!(stem, "mod" | "lib" | "main") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    for cand in [base.join(format!("{module}.rs")), base.join(module).join("mod.rs")] {
        if root.join(&cand).is_file() {
            return Some(cand);
        }
    }
    None
}

/// Repo-relative `.rs` files, gitignore-aware, skipping `target/`.
fn rust_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !rel.starts_with("target/") {
                out.push(rel);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn structure_extracts_items_and_methods() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.rs"),
            "pub struct Foo { x: i32 }\nimpl Foo {\n  pub fn bar(&self, n: i32) -> i32 { n + self.x }\n}\nfn top() {}\n",
        )
        .unwrap();
        let p = RustProvider::new(root);
        let nodes = p.structure(Path::new("a.rs")).unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"a.rs#Foo"), "struct: {ids:?}");
        assert!(ids.contains(&"a.rs#Foo.bar"), "method qualified by impl type: {ids:?}");
        assert!(ids.contains(&"a.rs#top"), "top-level fn: {ids:?}");
        // the method carries params/return/body sub-nodes
        let bar = nodes.iter().find(|n| n.id == "a.rs#Foo.bar").unwrap();
        let kinds: Vec<&str> = bar
            .children
            .iter()
            .filter_map(|c| match &c.kind {
                NodeKind::Syntax(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&"body") && kinds.contains(&"returnType"), "sub-nodes: {kinds:?}");
    }

    #[test]
    fn import_graph_follows_mod_decls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/sub")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod foo;\nmod sub;\n").unwrap();
        fs::write(root.join("src/foo.rs"), "pub fn f() {}\n").unwrap();
        fs::write(root.join("src/sub.rs"), "// dir module\n").unwrap(); // sub.rs OR sub/mod.rs
        let p = RustProvider::new(root);
        let g = p.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("src/lib.rs")).expect("lib.rs edges");
        assert!(edges.contains(&PathBuf::from("src/foo.rs")), "mod foo -> foo.rs: {edges:?}");
    }
}
