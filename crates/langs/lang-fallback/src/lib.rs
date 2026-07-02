//! lang-fallback — the GENERIC tree-sitter [`LanguageProvider`]: any language without a native
//! SCIP/LSP integration falls back here (Python, Go, Java, Ruby, C, C++ today). It delivers
//! the read path — `structure()` (functions / types / methods + fn sub-nodes), skeletal
//! `outline()`, and (Python) `import_graph()` — entirely in-process, plus **UNGATED**
//! structural edits.
//!
//! Two collectors: Python keeps its specialized walk (docstrings, decorated defs, dotted-name
//! import resolution); every other language shares ONE generic collector driven by the
//! tree-sitter field convention (`name` / `parameters` / `body`) plus a small per-language
//! kind table — adding a language is a grammar dependency + a few table rows, no new walker.
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

/// A language served by the tree-sitter fallback. Adding one = a grammar dependency, a variant
/// here, and rows in [`classify`]/[`outline`]'s tables — no core changes, no new walker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FbLang {
    Python,
    Js,
    /// TypeScript through the GENERIC provider — the `CI_TS_MODE` ablation arms only. NOT in
    /// [`ALL`]: `.ts` files normally belong to lang-ts, so detection/outline dispatch must
    /// never route them here; this variant is constructed explicitly by the registry builders.
    Ts,
    Go,
    Java,
    Ruby,
    C,
    Cpp,
}

pub const ALL: &[FbLang] = &[FbLang::Python, FbLang::Js, FbLang::Go, FbLang::Java, FbLang::Ruby, FbLang::C, FbLang::Cpp];

impl FbLang {
    /// Pick a fallback language for `root` by the source files actually present.
    pub fn detect(root: &Path) -> Option<FbLang> {
        ALL.iter().copied().find(|l| l.exts().iter().any(|e| has_ext(root, e)))
    }

    pub fn from_name(name: &str) -> Option<FbLang> {
        match name {
            "python" | "py" => Some(FbLang::Python),
            "js" | "javascript" => Some(FbLang::Js),
            "ts-fallback" => Some(FbLang::Ts), // ablation arms only — never plain "ts"
            "go" => Some(FbLang::Go),
            "java" => Some(FbLang::Java),
            "ruby" | "rb" => Some(FbLang::Ruby),
            "c" => Some(FbLang::C),
            "cpp" | "c++" | "cxx" => Some(FbLang::Cpp),
            _ => None,
        }
    }

    /// The fallback language owning `ext`, if any (the outline/read dispatch key).
    pub fn from_ext(ext: &str) -> Option<FbLang> {
        ALL.iter().copied().find(|l| l.exts().contains(&ext))
    }

    fn ts_language(self) -> tree_sitter::Language {
        match self {
            FbLang::Python => tree_sitter_python::LANGUAGE.into(),
            FbLang::Js => tree_sitter_javascript::LANGUAGE.into(),
            FbLang::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            FbLang::Go => tree_sitter_go::LANGUAGE.into(),
            FbLang::Java => tree_sitter_java::LANGUAGE.into(),
            FbLang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            FbLang::C => tree_sitter_c::LANGUAGE.into(),
            FbLang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        }
    }

    fn exts(self) -> &'static [&'static str] {
        match self {
            FbLang::Python => &["py", "pyi"],
            FbLang::Js => &["js", "jsx", "mjs", "cjs"],
            FbLang::Ts => &["ts", "mts", "cts"],
            FbLang::Go => &["go"],
            FbLang::Java => &["java"],
            FbLang::Ruby => &["rb"],
            FbLang::C => &["c", "h"],
            FbLang::Cpp => &["cpp", "cc", "cxx", "hpp", "hh"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FbLang::Python => "python",
            FbLang::Js => "javascript",
            FbLang::Ts => "typescript (tree-sitter ablation)",
            FbLang::Go => "go",
            FbLang::Java => "java",
            FbLang::Ruby => "ruby",
            FbLang::C => "c",
            FbLang::Cpp => "cpp",
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

    /// Fallback edits are never type-checked — there's no compiler/LSP behind them. The MCP
    /// layer reports this as `gated: false` so the agent knows the edit is structural only.
    fn gated(&self) -> bool {
        false
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
        match self.lang {
            // Python keeps its specialized walk (docstrings, decorated definitions).
            FbLang::Python => collect_items(tree.root_node(), bytes, &prefix, SymbolKind::Function, &mut out),
            // Everything else shares the generic field-convention collector.
            lang => collect_generic(lang, tree.root_node(), bytes, &prefix, &mut out),
        }
        Ok(out)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        // Import resolution exists where the syntax makes it cheap and reliable: Python
        // (dotted modules) and JS/TS (relative specifiers). Other fallback languages honestly
        // report NO edges (retrieval still works — graph expansion just doesn't) rather than
        // guessing edges from partially-understood import syntax.
        if !matches!(self.lang, FbLang::Python | FbLang::Js | FbLang::Ts) {
            return Ok(BTreeMap::new());
        }
        let mut graph: ImportGraph = BTreeMap::new();
        for ext in self.lang.exts() {
            for rel in source_files(&self.root, ext) {
                let Ok(content) = std::fs::read_to_string(self.root.join(&rel)) else { continue };
                let Some(tree) = self.parse(&content) else { continue };
                let mut edges: Vec<PathBuf> = Vec::new();
                match self.lang {
                    FbLang::Python => collect_imports(tree.root_node(), content.as_bytes(), &rel, &self.root, &mut edges),
                    _ => collect_js_imports(tree.root_node(), content.as_bytes(), &rel, &self.root, &mut edges),
                }
                edges.sort();
                edges.dedup();
                if !edges.is_empty() {
                    graph.insert(PathBuf::from(&rel), edges);
                }
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
    // Fold every function-like body; Python's placeholder is `...` (valid, idiomatic), brace
    // languages keep `elide_bodies`' default.
    let fn_kinds: &[&str] = match lang {
        FbLang::Python => &["function_definition"],
        FbLang::Js | FbLang::Ts => &["function_declaration", "generator_function_declaration", "method_definition"],
        FbLang::Go => &["function_declaration", "method_declaration"],
        FbLang::Java => &["method_declaration", "constructor_declaration"],
        FbLang::Ruby => &["method", "singleton_method"],
        FbLang::C | FbLang::Cpp => &["function_definition"],
    };
    let bodies = ci_treesitter::body_ranges(tree.root_node(), fn_kinds, &[]);
    if lang == FbLang::Python {
        ci_core::elide_bodies_with(content, bodies, "...")
    } else {
        ci_core::elide_bodies(content, bodies)
    }
}

// ── the generic collector (every fallback language except Python) ───────────

/// What a matched node kind IS, per language: a function/method (emits with sub-nodes, no
/// recursion into its body), a named type (leaf — struct/enum/type alias), or a container
/// (class/module/namespace — emits, then recurses into its body with a qualified prefix).
enum Shape {
    Fn,
    Type,
    Container,
}

/// The per-language kind table — the ONLY language-specific part of the generic collector.
fn classify(lang: FbLang, kind: &str) -> Option<Shape> {
    use Shape::*;
    Some(match (lang, kind) {
        // JS (grammar includes JSX): classes qualify their methods; arrow-function consts are
        // variable declarators (no name field on the function) and stay out — same tradeoff as
        // scip's Term handling, revisit if it bites.
        (FbLang::Js | FbLang::Ts, "function_declaration" | "generator_function_declaration" | "method_definition") => Fn,
        (FbLang::Js | FbLang::Ts, "class_declaration") => Container,
        (FbLang::Ts, "interface_declaration" | "enum_declaration" | "abstract_class_declaration") => Container,
        (FbLang::Ts, "type_alias_declaration") => Type,
        (FbLang::Go, "function_declaration" | "method_declaration") => Fn,
        (FbLang::Go, "type_spec") => Type,
        (FbLang::Java, "method_declaration" | "constructor_declaration") => Fn,
        (FbLang::Java, "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration") => Container,
        (FbLang::Ruby, "method" | "singleton_method") => Fn,
        (FbLang::Ruby, "class" | "module") => Container,
        (FbLang::C | FbLang::Cpp, "function_definition") => Fn,
        (FbLang::C | FbLang::Cpp, "struct_specifier" | "enum_specifier" | "union_specifier") => Type,
        (FbLang::Cpp, "class_specifier") => Container,
        (FbLang::Cpp, "namespace_definition") => Container,
        _ => return None,
    })
}

/// The definition's name node: the `name` field, else (C/C++) the first identifier-ish node
/// down the `declarator` chain (`function_definition → function_declarator → identifier`).
fn def_name<'a>(node: &TsNode<'a>) -> Option<TsNode<'a>> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some(n);
    }
    let mut d = node.child_by_field_name("declarator")?;
    for _ in 0..6 {
        if d.kind().ends_with("identifier") {
            return Some(d);
        }
        d = d.child_by_field_name("declarator")?;
    }
    None
}

/// Walk the tree emitting definitions per [`classify`]. Function bodies are NOT descended into
/// (locals are not symbols); container bodies are, with a `Container.` qualified prefix.
/// Unmatched nodes are transparent wrappers (declaration lists, export statements, preproc…).
fn collect_generic(lang: FbLang, node: TsNode, bytes: &[u8], prefix: &str, out: &mut Vec<Node>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let Some(shape) = classify(lang, child.kind()) else {
            collect_generic(lang, child, bytes, prefix, out);
            continue;
        };
        let Some(name_node) = def_name(&child) else { continue };
        let Ok(name) = name_node.utf8_text(bytes) else { continue };
        // A C `struct Foo x;` mentions the kind without a body — only DEFINITIONS count. Two
        // bodyless exceptions ARE definitions: go's `type_spec` (payload in its `type` field,
        // only appears inside `type_declaration`) and TS's `type_alias_declaration` (payload
        // in its `value` field).
        let is_definition = child.child_by_field_name("body").is_some()
            || matches!(shape, Shape::Fn)
            || (lang == FbLang::Go && child.kind() == "type_spec")
            || (lang == FbLang::Ts && child.kind() == "type_alias_declaration");
        if !is_definition {
            continue;
        }
        // Inside a container the prefix is `file#Scope.` — a trailing `.` marks a member.
        let kind = match shape {
            Shape::Fn if prefix.ends_with('.') => SymbolKind::Method,
            Shape::Fn => SymbolKind::Function,
            Shape::Type | Shape::Container => SymbolKind::Class,
        };
        let mut n = Node {
            id: format!("{prefix}{name}"),
            name: Some(name.to_string()),
            kind: NodeKind::Symbol(kind),
            range: ts_range(&child),
            name_range: Some(ts_range(&name_node)),
            children: vec![],
        };
        // Leading comment → the `:doc` anchor (parity with the gated providers). The comment
        // may sit above a single-child WRAPPER instead (`// doc` above go's `type Bucket …`
        // annotates the `type_declaration`, we emit its inner `type_spec`) — climb one level
        // when the parent wraps exactly this definition.
        let is_comment = |c: &TsNode| matches!(c.kind(), "comment" | "line_comment" | "block_comment");
        let doc = ci_treesitter::leading_comment_range(&child, is_comment).or_else(|| {
            child
                .parent()
                .filter(|p| p.named_child_count() == 1)
                .and_then(|p| ci_treesitter::leading_comment_range(&p, is_comment))
        });
        if let Some(r) = doc {
            n.children.push(Node {
                id: format!("{}:doc", n.id),
                name: None,
                kind: NodeKind::Syntax("doc".into()),
                range: r,
                name_range: None,
                children: vec![],
            });
        }
        match shape {
            Shape::Fn => {
                add_fn_subnodes(&mut n, &child, bytes);
                out.push(n);
            }
            Shape::Type => out.push(n),
            Shape::Container => {
                let inner = format!("{prefix}{name}.");
                out.push(n);
                if let Some(body) = child.child_by_field_name("body") {
                    collect_generic(lang, body, bytes, &inner, out);
                }
            }
        }
    }
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

    fn will_rename(&mut self, _from: &str, _to: &str) -> Result<Value> {
        Ok(json!({})) // no importer-rewrite engine; the move proceeds, blast radius is ungated
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
    // The parameter list: a `parameters` field, else (C/C++) the one inside the declarator
    // chain (`function_definition → function_declarator(parameters: …)`).
    let params_node = item.child_by_field_name("parameters").or_else(|| {
        let mut d = item.child_by_field_name("declarator")?;
        for _ in 0..6 {
            if let Some(p) = d.child_by_field_name("parameters") {
                return Some(p);
            }
            d = d.child_by_field_name("declarator")?;
        }
        None
    });
    if let Some(params) = params_node {
        // The whole `(...)` list — the insertion anchor for `add_parameter` / a missing return type.
        n.children.push(syntax_node(&format!("{}:params", n.id), None, "params", &params));
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
    // Return type field name varies by grammar: `return_type` (python/ruby), `result` (go),
    // `type` (java's method_declaration return).
    if let Some(rt) = item
        .child_by_field_name("return_type")
        .or_else(|| item.child_by_field_name("result"))
        .or_else(|| item.child_by_field_name("type"))
    {
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

/// JS/TS import edges from RELATIVE specifiers (`import … from './x'`, `export … from '../y'`).
/// Bare specifiers are packages (skipped); TS convention `./x.js` resolves to `./x.ts`. Resolution
/// tries the specifier as written, each source extension, then `index.<ext>` — misses cost an
/// edge, never invent one.
fn collect_js_imports(node: TsNode, bytes: &[u8], from_rel: &str, root: &Path, out: &mut Vec<PathBuf>) {
    if matches!(node.kind(), "import_statement" | "export_statement") {
        if let Some(src) = node.child_by_field_name("source") {
            let spec = src.utf8_text(bytes).unwrap_or("").trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string();
            if spec.starts_with("./") || spec.starts_with("../") {
                if let Some(p) = resolve_js_specifier(root, from_rel, &spec) {
                    out.push(p);
                }
            }
        }
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_js_imports(ch, bytes, from_rel, root, out);
    }
}

fn resolve_js_specifier(root: &Path, from_rel: &str, spec: &str) -> Option<PathBuf> {
    // Lexically normalize `./` and `../` so graph keys stay clean repo-relative paths.
    let joined = Path::new(from_rel).parent().unwrap_or(Path::new("")).join(spec);
    let mut base = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                base.pop();
            }
            std::path::Component::CurDir => {}
            other => base.push(other),
        }
    }
    // `./x.js` in TS source means `./x.ts` on disk — strip a source extension before probing.
    let stripped = ["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts"]
        .iter()
        .find(|e| base.extension().and_then(|x| x.to_str()) == Some(**e))
        .map(|_| base.with_extension(""))
        .unwrap_or_else(|| base.clone());
    const EXTS: [&str; 8] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];
    let mut candidates = vec![base.clone()];
    for e in EXTS {
        candidates.push(stripped.with_extension(e));
    }
    for e in EXTS {
        candidates.push(stripped.join(format!("index.{e}")));
    }
    candidates.into_iter().find(|c| root.join(c).is_file()).map(|c| norm(&c))
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

    /// One structure() round-trip per generic language: write a source file, assert the
    /// expected ids/kinds/sub-nodes come out of the shared collector.
    fn generic_structure(lang: FbLang, file: &str, content: &str) -> Vec<Node> {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(file), content).unwrap();
        FallbackProvider::new(dir.path(), lang).structure(Path::new(file)).unwrap()
    }

    #[test]
    fn js_structure_functions_classes_methods() {
        let nodes = generic_structure(
            FbLang::Js,
            "app.js",
            "// Formats a duration for display.\nfunction formatSpan(ms) {\n  return ms + 'ms';\n}\n\nclass Panel {\n  render(rows) {\n    return rows.map(formatSpan);\n  }\n}\n",
        );
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"app.js#formatSpan"), "js function: {ids:?}");
        assert!(ids.contains(&"app.js#Panel"), "js class: {ids:?}");
        assert!(ids.contains(&"app.js#Panel.render"), "js method qualified: {ids:?}");
        let f = nodes.iter().find(|n| n.id == "app.js#formatSpan").unwrap();
        assert!(f.children.iter().any(|c| c.id.ends_with(":body")), "js body sub-node");
        assert!(f.children.iter().any(|c| c.id.ends_with(":doc")), "leading comment -> :doc");
    }

    // The tree-sitter SYNTAX gate: an edit that no longer parses must REJECT (tree-sitter
    // never refuses input, so "unparseable" = new ERROR/missing nodes, baseline-diffed), and
    // a file's PRE-EXISTING breakage must never block an unrelated edit.
    #[test]
    fn syntax_gate_rejects_broken_edits_but_not_preexisting_breakage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("svc.go"), "package svc\n\nfunc probe(url string) bool {\n\treturn true\n}\n").unwrap();
        // A SECOND file that is already broken — must not block edits to svc.go.
        fs::write(root.join("broken.go"), "package svc\n\nfunc oops( {\n").unwrap();
        let p = FallbackProvider::new(root, FbLang::Go);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        // Unbalanced-brace body -> REJECTED, disk untouched.
        let before = fs::read_to_string(root.join("svc.go")).unwrap();
        let bad = p
            .apply_edits(&[EditOp::SetBody { node_id: "svc.go#probe".into(), body: "{\n\treturn (true\n".into() }], &opts)
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "broken syntax must reject: {bad:?}");
        assert_eq!(fs::read_to_string(root.join("svc.go")).unwrap(), before, "disk untouched on reject");

        // A clean edit to the same file still commits (broken.go's errors are baseline).
        let ok = p
            .apply_edits(&[EditOp::SetBody { node_id: "svc.go#probe".into(), body: "{\n\treturn false\n}".into() }], &opts)
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit commits despite unrelated pre-existing breakage: {ok:?}");
        assert!(fs::read_to_string(root.join("svc.go")).unwrap().contains("return false"));

        // Editing the BROKEN file without fixing it: its old errors are baseline, so a clean
        // structural change... skip — its structure is unparseable; the contract that matters
        // is above: pre-existing breakage elsewhere never blocks.
    }

    // The CI_TS_MODE ablation read path: TS through the generic collector, plus the JS/TS
    // relative-import resolver (`./x.js` -> `x.ts`, index files, `..` normalization).
    #[test]
    fn ts_ablation_structure_and_import_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/util")).unwrap();
        std::fs::write(
            root.join("src/rank.ts"),
            "import { clamp } from \"./util/math.js\";\nexport interface RankRow {\n  score: number;\n}\nexport type RankFn = (r: RankRow) => number;\nexport class Ranker {\n  top(rows: RankRow[]): RankRow[] {\n    return rows;\n  }\n}\nexport function rankAll(rows: RankRow[]): number {\n  return clamp(rows.length);\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/util/math.ts"), "export function clamp(x: number): number {\n  return x;\n}\n").unwrap();

        let prov = FallbackProvider::new(root, FbLang::Ts);
        let nodes = prov.structure(Path::new("src/rank.ts")).unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"src/rank.ts#RankRow"), "interface: {ids:?}");
        assert!(ids.contains(&"src/rank.ts#RankFn"), "type alias (bodyless definition): {ids:?}");
        assert!(ids.contains(&"src/rank.ts#Ranker.top"), "method qualified: {ids:?}");
        assert!(ids.contains(&"src/rank.ts#rankAll"), "function: {ids:?}");

        let g = prov.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("src/rank.ts")).expect("rank.ts edges");
        assert_eq!(edges, &vec![PathBuf::from("src/util/math.ts")], ".js specifier resolved to .ts: {edges:?}");
    }

    #[test]
    fn go_structure_functions_methods_types() {
        let nodes = generic_structure(
            FbLang::Go,
            "svc.go",
            "package svc\n\n// Latency bucket.\ntype Bucket struct {\n\tp99 float64\n}\n\nfunc Probe(url string) bool {\n\treturn true\n}\n\nfunc (b Bucket) Worst() float64 {\n\treturn b.p99\n}\n",
        );
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"svc.go#Bucket"), "go type: {ids:?}");
        assert!(ids.contains(&"svc.go#Probe"), "go func: {ids:?}");
        assert!(ids.contains(&"svc.go#Worst"), "go method: {ids:?}");
        let probe = nodes.iter().find(|n| n.id == "svc.go#Probe").unwrap();
        let kinds: Vec<&str> = probe.children.iter().filter_map(|c| match &c.kind { NodeKind::Syntax(s) => Some(s.as_str()), _ => None }).collect();
        assert!(kinds.contains(&"body") && kinds.contains(&"params"), "go sub-nodes: {kinds:?}");
        let bucket = nodes.iter().find(|n| n.id == "svc.go#Bucket").unwrap();
        assert!(bucket.children.iter().any(|c| c.id.ends_with(":doc")), "leading comment -> :doc: {:?}", bucket.children);
    }

    #[test]
    fn java_structure_class_members_qualified() {
        let nodes = generic_structure(
            FbLang::Java,
            "Svc.java",
            "public class Svc {\n  private int hits;\n  public Svc() {}\n  public int probe(String url) {\n    return 1;\n  }\n}\n",
        );
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"Svc.java#Svc"), "class: {ids:?}");
        assert!(ids.contains(&"Svc.java#Svc.probe"), "method qualified by class: {ids:?}");
        let probe = nodes.iter().find(|n| n.id == "Svc.java#Svc.probe").unwrap();
        assert!(matches!(probe.kind, NodeKind::Symbol(SymbolKind::Method)), "member kind: {:?}", probe.kind);
        assert!(probe.children.iter().any(|c| c.id.ends_with(":body")), "java body sub-node");
        assert!(!ids.contains(&"Svc.java#hits"), "fields are not emitted (no local noise): {ids:?}");
    }

    #[test]
    fn ruby_structure_class_and_methods() {
        let nodes = generic_structure(
            FbLang::Ruby,
            "svc.rb",
            "class Svc\n  def probe(url)\n    true\n  end\nend\n\ndef helper\n  1\nend\n",
        );
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"svc.rb#Svc"), "ruby class: {ids:?}");
        assert!(ids.contains(&"svc.rb#Svc.probe"), "ruby method qualified: {ids:?}");
        assert!(ids.contains(&"svc.rb#helper"), "ruby top-level def: {ids:?}");
    }

    #[test]
    fn c_structure_functions_and_structs() {
        let nodes = generic_structure(
            FbLang::C,
            "probe.c",
            "struct bucket {\n  double p99;\n};\n\nstatic int probe(const char *url) {\n  return 1;\n}\n\nstruct bucket use_only(struct bucket b) {\n  return b;\n}\n",
        );
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"probe.c#bucket"), "struct definition: {ids:?}");
        assert!(ids.contains(&"probe.c#probe"), "fn name via declarator descent: {ids:?}");
        assert!(ids.contains(&"probe.c#use_only"), "fn returning a struct: {ids:?}");
        // `struct bucket` in the parameter/return positions must NOT re-emit the type.
        assert_eq!(ids.iter().filter(|i| **i == "probe.c#bucket").count(), 1, "reference != definition: {ids:?}");
        let probe = nodes.iter().find(|n| n.id == "probe.c#probe").unwrap();
        assert!(probe.children.iter().any(|c| c.id.ends_with(":params")), "params found inside declarator");
    }

    #[test]
    fn cpp_structure_class_namespace() {
        let nodes = generic_structure(
            FbLang::Cpp,
            "svc.cpp",
            "namespace net {\nclass Svc {\n public:\n  int probe();\n};\nint Svc::probe() { return 1; }\n}\n",
        );
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"svc.cpp#net"), "namespace: {ids:?}");
        assert!(ids.contains(&"svc.cpp#net.Svc"), "class in namespace: {ids:?}");
    }

    #[test]
    fn generic_outline_folds_function_bodies() {
        let o = outline(FbLang::Go, "package a\n\nfunc big() int {\n\tx := 1\n\ty := 2\n\treturn x + y\n}\n");
        assert!(o.contains("func big() int"), "signature kept: {o}");
        assert!(!o.contains("x := 1"), "body folded: {o}");
    }

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

    #[test]
    fn ungated_add_parameter_and_return_type() {
        let dir = py_project();
        let root = dir.path();
        let p = FallbackProvider::new(root, FbLang::Python);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        // Append a parameter, then (in a second batch, so structure re-parses the updated file)
        // add a return type where none exists (`-> T` for Python).
        let add = p
            .apply_edits(&[EditOp::AddParameter { node_id: "pkg/math_utils.py#add".into(), param: "c".into() }], &opts)
            .unwrap();
        assert!(matches!(add, CommitResult::Ok { .. }), "add_parameter must commit: {add:?}");
        let ret = p
            .apply_edits(&[EditOp::SetReturnType { node_id: "pkg/math_utils.py#add".into(), ty: "int".into() }], &opts)
            .unwrap();
        assert!(matches!(ret, CommitResult::Ok { .. }), "set_return_type must commit: {ret:?}");
        let after = fs::read_to_string(root.join("pkg/math_utils.py")).unwrap();
        assert!(after.contains("def add(a, b, c) -> int:"), "param appended + return type added: {after}");
    }

    #[test]
    fn ungated_insert_in_body_python_suite() {
        let dir = py_project();
        let root = dir.path();
        let p = FallbackProvider::new(root, FbLang::Python);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        // A Python suite has no closing brace: the statement appends after the last body line,
        // matching its indentation.
        let res = p
            .apply_edits(
                &[EditOp::InsertInBody {
                    node_id: "pkg/math_utils.py#Calc.total".into(),
                    code: "print(xs)".into(),
                    after: None,
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "insert_in_body must commit: {res:?}");
        let after = fs::read_to_string(root.join("pkg/math_utils.py")).unwrap();
        assert!(
            after.contains("        return sum(xs)\n        print(xs)"),
            "statement appended into the suite at the right indent: {after}"
        );
    }
}
