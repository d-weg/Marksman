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
//! to the gated [`ci_edit::GateEngine`] path as its LSP/indexer lands (the Rust provider is
//! the model).
use ci_core::{
    rel_path, CommitResult, EditOp, EditOpts, Granularity, ImportGraph, LanguageProvider, Node,
    Result, SymbolKind,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tree_sitter::Parser;

mod gate;
mod imports;
mod structure;

/// Java syntactic resolution primitives, exposed so `lang-java`'s move model speaks the SAME
/// resolver as the import graph (contract §7: one source of truth, no divergent reimplementation).
/// `file_to_fqn` inverts a path to its FQN, `resolve_import` maps an `import` to a file, and
/// `package_decl` is the shared package-declaration scanner (line index + name) both the move
/// model's membership edits and the resolver's `package_of` key off.
pub mod java {
    pub use crate::imports::{file_to_fqn, package_decl, resolve_import};
}

/// PHP syntactic resolution primitives, exposed so `lang-php`'s move model speaks the SAME
/// PSR-4 resolver as the import graph (contract §7: one source of truth). `file_to_fqcn`
/// inverts a path to its namespaced FQCN, `resolve_use` maps a `use` FQCN to a file, and
/// `namespace_decl` is the shared namespace-declaration scanner (line index + name) both the
/// move model's membership edits and the resolver's `namespace_of` key off.
pub mod php {
    pub use crate::imports::{file_to_fqcn, namespace_decl, resolve_use};
}

/// A language served by the tree-sitter fallback. Adding one = a grammar dependency, a variant
/// here, and rows in `classify` (structure.rs) / [`outline`]'s tables — no core changes, no
/// new walker.
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
    Php,
    Ruby,
    C,
    Cpp,
    Swift,
}

pub const ALL: &[FbLang] = &[FbLang::Python, FbLang::Js, FbLang::Go, FbLang::Java, FbLang::Php, FbLang::Ruby, FbLang::C, FbLang::Cpp, FbLang::Swift];

impl FbLang {
    /// Pick a fallback language for `root` by the source files actually present.
    pub fn detect(root: &Path) -> Option<FbLang> {
        ALL.iter().copied().find(|l| l.exts().iter().any(|e| imports::has_ext(root, e)))
    }

    pub fn from_name(name: &str) -> Option<FbLang> {
        match name {
            "python" | "py" => Some(FbLang::Python),
            "js" | "javascript" => Some(FbLang::Js),
            "ts-fallback" => Some(FbLang::Ts), // ablation arms only — never plain "ts"
            "go" => Some(FbLang::Go),
            "java" => Some(FbLang::Java),
            "php" => Some(FbLang::Php),
            "ruby" | "rb" => Some(FbLang::Ruby),
            "c" => Some(FbLang::C),
            "cpp" | "c++" | "cxx" => Some(FbLang::Cpp),
            "swift" => Some(FbLang::Swift),
            _ => None,
        }
    }

    /// The fallback language owning `ext`, if any (the outline/read dispatch key).
    pub fn from_ext(ext: &str) -> Option<FbLang> {
        ALL.iter().copied().find(|l| l.exts().contains(&ext))
    }

    pub(crate) fn ts_language(self) -> tree_sitter::Language {
        match self {
            FbLang::Python => tree_sitter_python::LANGUAGE.into(),
            FbLang::Js => tree_sitter_javascript::LANGUAGE.into(),
            FbLang::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            FbLang::Go => tree_sitter_go::LANGUAGE.into(),
            FbLang::Java => tree_sitter_java::LANGUAGE.into(),
            // The FULL PHP grammar (not php_only): it handles HTML-interleaved `.php` files,
            // which the single-language grammar rejects.
            FbLang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            FbLang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            FbLang::C => tree_sitter_c::LANGUAGE.into(),
            FbLang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            FbLang::Swift => tree_sitter_swift::LANGUAGE.into(),
        }
    }

    pub(crate) fn exts(self) -> &'static [&'static str] {
        match self {
            FbLang::Python => &["py", "pyi"],
            FbLang::Js => &["js", "jsx", "mjs", "cjs"],
            FbLang::Ts => &["ts", "mts", "cts"],
            FbLang::Go => &["go"],
            FbLang::Java => &["java"],
            FbLang::Php => &["php"],
            FbLang::Ruby => &["rb"],
            FbLang::C => &["c", "h"],
            FbLang::Cpp => &["cpp", "cc", "cxx", "hpp", "hh"],
            FbLang::Swift => &["swift"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FbLang::Python => "python",
            FbLang::Js => "javascript",
            FbLang::Ts => "typescript (tree-sitter ablation)",
            FbLang::Go => "go",
            FbLang::Java => "java",
            FbLang::Php => "php",
            FbLang::Ruby => "ruby",
            FbLang::C => "c",
            FbLang::Cpp => "cpp",
            FbLang::Swift => "swift",
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

    /// Outgoing import edges for ONE file — the per-file slice of `import_graph()`, so a
    /// post-commit freshness refresh parses just the changed files instead of re-walking
    /// the whole repo. Languages without cheap-and-reliable resolution return no edges,
    /// same as the whole-graph path.
    pub fn file_imports(&self, rel: &str) -> Vec<PathBuf> {
        if !matches!(self.lang, FbLang::Python | FbLang::Js | FbLang::Ts | FbLang::Java | FbLang::Php) {
            return Vec::new();
        }
        let Ok(content) = std::fs::read_to_string(self.root.join(rel)) else { return Vec::new() };
        let Some(tree) = self.parse(&content) else { return Vec::new() };
        let mut edges: Vec<PathBuf> = Vec::new();
        match self.lang {
            FbLang::Python => imports::collect_imports(tree.root_node(), content.as_bytes(), rel, &self.root, &mut edges),
            FbLang::Java => imports::collect_java_imports(tree.root_node(), content.as_bytes(), rel, &self.root, &mut edges),
            FbLang::Php => imports::collect_php_imports(tree.root_node(), content.as_bytes(), rel, &self.root, &mut edges),
            _ => imports::collect_js_imports(tree.root_node(), content.as_bytes(), rel, &self.root, &mut edges),
        }
        edges.sort();
        edges.dedup();
        edges
    }
}

/// The tree-sitter reader as a composable READ half (see `ci_core::ReadIndex`): live (parses
/// current disk — no post-commit freshness glue needed) with syntactic edges (a composed
/// gate must expand the radius transitively; bench T9).
impl ci_core::ReadIndex for FallbackProvider {
    fn granularity(&self) -> Granularity {
        LanguageProvider::granularity(self)
    }
    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        LanguageProvider::structure(self, file)
    }
    fn import_graph(&self) -> Result<ImportGraph> {
        LanguageProvider::import_graph(self)
    }
    fn live(&self) -> bool {
        true
    }
    fn semantic_edges(&self) -> bool {
        false
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
        let rel = rel_path(&self.root, file);
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
            FbLang::Python => structure::collect_items(tree.root_node(), bytes, &prefix, SymbolKind::Function, &mut out),
            // Everything else shares the generic field-convention collector.
            lang => structure::collect_generic(lang, tree.root_node(), bytes, &prefix, &mut out),
        }
        Ok(out)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        // Import resolution exists where the syntax makes it cheap and reliable: Python
        // (dotted modules), JS/TS (relative specifiers), Java (`import a.b.C` →
        // `<source-root>/a/b/C.java`, package-path-bound), and PHP (`use A\B\C` via the
        // composer.json PSR-4 map). Other fallback languages honestly report NO edges
        // (retrieval still works — graph expansion just doesn't) rather than guessing edges
        // from partially-understood import syntax.
        let mut graph: ImportGraph = BTreeMap::new();
        for ext in self.lang.exts() {
            for rel in imports::source_files(&self.root, ext) {
                let edges = self.file_imports(&rel);
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
        let mut engine = gate::NoGate::new(&self.root, self.lang);
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
        FbLang::Php => &["function_definition", "method_declaration"],
        FbLang::Ruby => &["method", "singleton_method"],
        FbLang::C | FbLang::Cpp => &["function_definition"],
        // Swift: the grammar names both free functions and members `function_declaration`;
        // `init_declaration` is the initializer, `protocol_function_declaration` the bodyless
        // requirement in a protocol (folding it is a no-op, harmless to list).
        FbLang::Swift => &["function_declaration", "init_declaration"],
    };
    let bodies = ci_treesitter::body_ranges(tree.root_node(), fn_kinds, &[]);
    if lang == FbLang::Python {
        ci_core::elide_bodies_with(content, bodies, "...")
    } else {
        ci_core::elide_bodies(content, bodies)
    }
}

/// Byte ranges `(start, end)` of every string-literal and comment node in `content`, per the
/// language grammar. The move rewriter uses this to NEVER retarget a fully-qualified name that
/// merely appears inside a string or comment (a `Class.forName("a.b.C")` reflection string, a doc
/// mention): a rewrite there still compiles, so the type-check gate can't catch it — excluding it
/// is the safe contract. Uses tree-sitter (not a hand-rolled string lexer) so heredocs, block
/// comments, interpolated and escaped strings are all covered exactly. Empty on a parse/grammar
/// failure — the caller then scans unmasked, no worse than before this guard existed.
pub fn string_comment_spans(lang: FbLang, content: &str) -> Vec<(usize, usize)> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else { return Vec::new() };
    let mut out = Vec::new();
    collect_string_comment(tree.root_node(), &mut out);
    out
}

/// Byte offset where each line begins (line 0 at 0, then just past each `\n`) — maps a movefix
/// `(line, column)` reference span to an absolute content offset for [`string_comment_spans`]
/// masking. CRLF-safe: only line STARTS are recorded, and a column is start-relative either way.
pub fn line_start_offsets(content: &str) -> Vec<usize> {
    std::iter::once(0).chain(content.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

fn collect_string_comment(node: tree_sitter::Node, out: &mut Vec<(usize, usize)>) {
    if is_string_or_comment(node.kind()) {
        out.push((node.start_byte(), node.end_byte()));
        return; // the whole node is masked — an interpolated var inside a string is no rewrite target
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        collect_string_comment(child, out);
    }
}

/// Grammar-version-robust string/comment classification: match on kind SUBSTRINGS so a renamed
/// node (`string_literal` vs `string` vs `encapsed_string`; `line_comment` vs `comment`) still
/// masks. Over-masking a rare coincidental kind is harmless — it only suppresses a name rewrite
/// there, and the move fallback is best-effort by design.
fn is_string_or_comment(kind: &str) -> bool {
    kind.contains("string")
        || kind.contains("comment")
        || kind == "heredoc"
        || kind == "nowdoc"
        || kind == "text_block"
        || kind == "character_literal"
        || kind == "char_literal"
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_core::NodeKind;
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

    // Named values are anchors (SCIP collects them as Term symbols; edit targets in every
    // language): TS class fields / interface properties / top-level consts, Go consts, Java
    // fields — with ranges widened to the declaration statement.
    #[test]
    fn field_and_const_anchors_across_languages() {
        let ts = generic_structure(
            FbLang::Ts,
            "cfg.ts",
            "export const K_DEFAULT = 1.5;\nexport interface Opts {\n  topN: number;\n}\nexport class Ranker {\n  k1 = 1.5;\n  run(): number {\n    return this.k1;\n  }\n}\n",
        );
        let ids: Vec<&str> = ts.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"cfg.ts#K_DEFAULT"), "top-level const: {ids:?}");
        assert!(ids.contains(&"cfg.ts#Opts.topN"), "interface property: {ids:?}");
        assert!(ids.contains(&"cfg.ts#Ranker.k1"), "class field: {ids:?}");
        let k = ts.iter().find(|n| n.id == "cfg.ts#K_DEFAULT").unwrap();
        assert!(matches!(k.kind, NodeKind::Symbol(SymbolKind::Variable)));
        assert!(k.range.end_char > 20, "range spans the whole declaration, not just the name: {:?}", k.range);

        let go = generic_structure(FbLang::Go, "cfg.go", "package cfg\n\nconst MaxRetries = 3\n\nvar timeout = 10\n");
        let ids: Vec<&str> = go.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"cfg.go#MaxRetries") && ids.contains(&"cfg.go#timeout"), "go const/var: {ids:?}");

        let java = generic_structure(FbLang::Java, "Cfg.java", "public class Cfg {\n  private int maxRetries = 3;\n}\n");
        let ids: Vec<&str> = java.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"Cfg.java#Cfg.maxRetries"), "java field via declarator name: {ids:?}");
    }

    // A JS/TS move REWRITES importers syntactically (and the moved file's own imports),
    // preserving specifier extension style — the fallback equivalent of willRenameFiles.
    #[test]
    fn js_move_rewrites_importers_and_own_imports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("app.js"), "import { fold } from \"./b.js\";\nexport const x = fold(1);\n").unwrap();
        std::fs::write(root.join("b.js"), "import { base } from \"./util.js\";\nexport function fold(n) {\n  return base + n;\n}\n").unwrap();
        std::fs::write(root.join("util.js"), "export const base = 1;\n").unwrap();

        let p = FallbackProvider::new(root, FbLang::Js);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let res = p
            .apply_edits(&[EditOp::MoveFile { from: "b.js".into(), to: "lib/b.js".into() }], &opts)
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "move commits: {res:?}");
        assert!(root.join("lib/b.js").exists() && !root.join("b.js").exists());

        let app = std::fs::read_to_string(root.join("app.js")).unwrap();
        assert!(app.contains("\"./lib/b.js\""), "importer retargeted (ext style kept): {app}");
        let moved = std::fs::read_to_string(root.join("lib/b.js")).unwrap();
        assert!(moved.contains("\"../util.js\""), "moved file's own import recomputed: {moved}");
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

    // The Java resolver: `import a.b.C` -> a real repo edge under the conventional source
    // root, and CONSERVATIVE per contract §3 — an external import (`java.util.List`, no
    // in-repo source) contributes NO edge, never a guessed one.
    #[test]
    fn java_import_graph_resolves_in_repo_and_invents_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/main/java/app")).unwrap();
        std::fs::create_dir_all(root.join("src/main/java/lib")).unwrap();
        std::fs::write(
            root.join("src/main/java/app/Svc.java"),
            "package app;\n\nimport lib.Dep;\nimport java.util.List;\n\npublic class Svc {\n  public int probe() {\n    return new Dep().value();\n  }\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main/java/lib/Dep.java"),
            "package lib;\n\npublic class Dep {\n  public int value() {\n    return 1;\n  }\n}\n",
        )
        .unwrap();

        let prov = FallbackProvider::new(root, FbLang::Java);
        let g = prov.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("src/main/java/app/Svc.java")).expect("Svc.java edges");
        // The in-repo import resolves; the external `java.util.List` is silently dropped.
        assert_eq!(
            edges,
            &vec![PathBuf::from("src/main/java/lib/Dep.java")],
            "only the in-repo import is an edge (no invented edge for java.util.List): {edges:?}"
        );
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

    // The tree-sitter-php grammar LOADS at the pinned core ABI (0.26.10) — a one-line parse
    // that would panic/return None on an ABI mismatch — and the FULL grammar parses
    // HTML-interleaved `.php` without choking. This is the spec's residual grammar-load check.
    #[test]
    fn php_grammar_loads_at_pinned_abi() {
        let dir = tempfile::tempdir().unwrap();
        let p = FallbackProvider::new(dir.path(), FbLang::Php);
        let tree = p.parse("<?php\nfunction f() { return 1; }\n?>\n<div>html</div>\n").expect("php parses");
        assert!(!tree.root_node().is_error(), "php root node parses clean (grammar ABI ok)");
    }

    #[test]
    fn php_structure_class_members_qualified() {
        let nodes = generic_structure(
            FbLang::Php,
            "Svc.php",
            "<?php\nnamespace App;\n\n// Probes the service.\nclass Svc {\n  private int $hits = 0;\n  const MAX = 5;\n  public function probe(string $url): int {\n    return 1;\n  }\n}\n",
        );
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"Svc.php#Svc"), "php class: {ids:?}");
        assert!(ids.contains(&"Svc.php#Svc.probe"), "php method qualified: {ids:?}");
        assert!(ids.iter().any(|i| i.starts_with("Svc.php#Svc.") && i.contains("hits")), "php property: {ids:?}");
        assert!(ids.contains(&"Svc.php#Svc.MAX"), "php const: {ids:?}");
        let probe = nodes.iter().find(|n| n.id == "Svc.php#Svc.probe").unwrap();
        assert!(matches!(probe.kind, NodeKind::Symbol(SymbolKind::Method)), "member kind: {:?}", probe.kind);
        assert!(probe.children.iter().any(|c| c.id.ends_with(":body")), "php body sub-node");
        assert!(probe.children.iter().any(|c| c.id.ends_with(":return")), "php return sub-node (suffix-typed)");
        let svc = nodes.iter().find(|n| n.id == "Svc.php#Svc").unwrap();
        assert!(svc.children.iter().any(|c| c.id.ends_with(":doc")), "leading comment -> :doc");
    }

    // Bare-return `function` at top level (no class) is a Function, not a Method.
    #[test]
    fn php_top_level_function() {
        let nodes = generic_structure(
            FbLang::Php,
            "helpers.php",
            "<?php\nfunction fold(int $n): int {\n  return $n;\n}\n",
        );
        let f = nodes.iter().find(|n| n.id == "helpers.php#fold").expect("top-level fn");
        assert!(matches!(f.kind, NodeKind::Symbol(SymbolKind::Function)), "top-level fn is Function: {:?}", f.kind);
    }

    // PSR-4: `use App\Lib\Dep;` under `"App\\": "src/"` resolves to `src/Lib/Dep.php`; a vendor
    // class (no PSR-4 prefix) and a repo with no composer.json contribute NO edge (contract §3).
    #[test]
    fn php_import_graph_psr4_resolves_and_invents_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/App/Lib")).unwrap();
        fs::write(
            root.join("composer.json"),
            "{\n  \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/App/\" } }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/App/Svc.php"),
            "<?php\nnamespace App;\nuse App\\Lib\\Dep;\nuse Psr\\Log\\LoggerInterface;\nclass Svc {}\n",
        )
        .unwrap();
        fs::write(root.join("src/App/Lib/Dep.php"), "<?php\nnamespace App\\Lib;\nclass Dep {}\n").unwrap();

        let p = FallbackProvider::new(root, FbLang::Php);
        let g = p.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("src/App/Svc.php")).expect("Svc.php edges");
        assert_eq!(
            edges,
            &vec![PathBuf::from("src/App/Lib/Dep.php")],
            "only the PSR-4-resolvable in-repo use is an edge (no invented edge for the vendor class): {edges:?}"
        );
    }

    #[test]
    fn php_import_graph_empty_without_composer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Svc.php"), "<?php\nuse App\\Lib\\Dep;\nclass Svc {}\n").unwrap();
        let p = FallbackProvider::new(root, FbLang::Php);
        assert!(p.import_graph().unwrap().is_empty(), "no composer.json => the honest empty graph");
    }

    // PSR-4 is EXCLUSIVE: the longest matching prefix OWNS the FQCN. When its mapped dir doesn't
    // hold the file, the class is unresolved — resolution must NOT fall through to a broader prefix
    // and land on a coincidental shadow file (an invented edge that inverts delete-safety, §3).
    #[test]
    fn php_psr4_longest_prefix_owns_no_fallthrough_to_shadow() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/Sub")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(
            root.join("composer.json"),
            "{ \"autoload\": { \"psr-4\": { \"App\\\\\": \"src/\", \"App\\\\Sub\\\\\": \"other/\" } } }\n",
        )
        .unwrap();
        // The SHADOW file lives under the broad `App\` prefix; the OWNER dir (`other/`) is empty.
        fs::write(root.join("src/Sub/Thing.php"), "<?php\nnamespace App\\Sub;\nclass Thing {}\n").unwrap();
        fs::write(
            root.join("src/Consumer.php"),
            "<?php\nnamespace App;\nuse App\\Sub\\Thing;\nclass Consumer {}\n",
        )
        .unwrap();

        let p = FallbackProvider::new(root, FbLang::Php);
        let g = p.import_graph().unwrap();
        assert!(
            g.get(&PathBuf::from("src/Consumer.php")).map(|e| e.is_empty()).unwrap_or(true),
            "App\\Sub\\ owns the FQCN; its dir is empty => unresolved, NOT the src/Sub/Thing.php shadow: {:?}",
            g.get(&PathBuf::from("src/Consumer.php"))
        );
    }

    // Java resolution must VERIFY the target declares the import's package: a file sitting at the
    // import's path but belonging to a DIFFERENT package is a path coincidence, not a dependency —
    // emitting the edge would invent one (contract §3), worst in the flat-root fallback.
    #[test]
    fn java_import_no_edge_on_package_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("util")).unwrap();
        fs::write(root.join("App.java"), "import util.Helper;\npublic class App {}\n").unwrap();
        // A file at util/Helper.java, but it declares a DIFFERENT package — not `util.Helper`.
        fs::write(root.join("util/Helper.java"), "package wrongpkg;\npublic class Helper {}\n").unwrap();

        let p = FallbackProvider::new(root, FbLang::Java);
        let g = p.import_graph().unwrap();
        assert!(
            g.get(&PathBuf::from("App.java")).map(|e| e.is_empty()).unwrap_or(true),
            "package mismatch => no edge (path coincidence, not a real dependency): {:?}",
            g.get(&PathBuf::from("App.java"))
        );
    }

    // The tree-sitter-swift grammar LOADS at the pinned core ABI (0.26.10) — a one-line parse
    // that would panic/return None on an ABI mismatch. The spec flags ABI-14-in-core-0.26 as
    // inference, so this load test is REQUIRED, not optional.
    #[test]
    fn swift_grammar_loads_at_pinned_abi() {
        let dir = tempfile::tempdir().unwrap();
        let p = FallbackProvider::new(dir.path(), FbLang::Swift);
        let tree = p.parse("import Foundation\nfunc f() -> Int { return 1 }\n").expect("swift parses");
        assert!(!tree.root_node().has_error(), "swift root node parses clean (grammar ABI ok)");
    }

    // Swift structure: the grammar collapses struct/class/enum/extension onto `class_declaration`
    // (protocol has its own node), members qualify under their container, `func`/`init` carry
    // `:body` + `:params` + a SUFFIX `:return`, a `var`/`let` property is a Field, and a leading
    // `///` comment surfaces as `:doc`.
    #[test]
    fn swift_structure_types_members_and_subnodes() {
        let nodes = generic_structure(
            FbLang::Swift,
            "Svc.swift",
            "/// A service.\nclass Svc {\n  var hits: Int = 0\n  func probe(url: String) -> Bool {\n    return true\n  }\n  init(x: Int) { self.hits = x }\n}\n\nstruct Bucket {\n  let p99: Double = 0\n}\n\nprotocol Pinger {\n  func ping() -> Int\n}\n\nenum Color { case red, green }\n\nfunc topLevel(n: Int) -> Int {\n  return n\n}\n",
        );
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"Svc.swift#Svc"), "class: {ids:?}");
        assert!(ids.contains(&"Svc.swift#Svc.probe"), "method qualified by class: {ids:?}");
        assert!(ids.contains(&"Svc.swift#Svc.hits"), "property qualified by class: {ids:?}");
        assert!(ids.contains(&"Svc.swift#Bucket"), "struct (also class_declaration): {ids:?}");
        assert!(ids.contains(&"Svc.swift#Bucket.p99"), "struct property: {ids:?}");
        assert!(ids.contains(&"Svc.swift#Pinger"), "protocol: {ids:?}");
        assert!(ids.contains(&"Svc.swift#Pinger.ping"), "protocol requirement: {ids:?}");
        assert!(ids.contains(&"Svc.swift#Color"), "enum (also class_declaration): {ids:?}");
        assert!(ids.contains(&"Svc.swift#Color.red"), "enum case via first enum_entry name: {ids:?}");
        assert!(ids.contains(&"Svc.swift#topLevel"), "top-level function: {ids:?}");

        let probe = nodes.iter().find(|n| n.id == "Svc.swift#Svc.probe").unwrap();
        assert!(matches!(probe.kind, NodeKind::Symbol(SymbolKind::Method)), "member kind: {:?}", probe.kind);
        assert!(probe.children.iter().any(|c| c.id.ends_with(":body")), "swift :body sub-node");
        assert!(probe.children.iter().any(|c| c.id.ends_with(":params")), "swift :params sub-node");
        assert!(probe.children.iter().any(|c| c.id.ends_with(":return")), "swift suffix :return sub-node");

        let svc = nodes.iter().find(|n| n.id == "Svc.swift#Svc").unwrap();
        assert!(svc.children.iter().any(|c| c.id.ends_with(":doc")), "leading /// comment -> :doc");
        let top = nodes.iter().find(|n| n.id == "Svc.swift#topLevel").unwrap();
        assert!(matches!(top.kind, NodeKind::Symbol(SymbolKind::Function)), "top-level fn is Function: {:?}", top.kind);
    }

    // Swift imports are MODULE-level (SwiftPM targets glob directories), so there are no
    // file-file edges to extract — the honest empty graph (contract §3, the module-model note in
    // the rollout spec). `import Foundation` contributes nothing, never a guessed edge.
    #[test]
    fn swift_import_graph_is_empty_by_design() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("A.swift"), "import Foundation\nstruct A {}\n").unwrap();
        fs::write(root.join("B.swift"), "struct B {}\n").unwrap();
        let p = FallbackProvider::new(root, FbLang::Swift);
        assert!(
            p.import_graph().unwrap().is_empty(),
            "module-level imports yield no file edges — the honest empty graph"
        );
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

    // Java import resolution over a MAVEN layout: `import a.b.C` under `src/main/java` resolves
    // to the file, a wildcard resolves to the package's files, and an EXTERNAL dependency
    // (`java.util.List`) produces zero edges (contract §3 — no invented edge).
    #[test]
    fn java_import_graph_maven_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/main/java/com/acme/util")).unwrap();
        fs::write(
            root.join("src/main/java/com/acme/App.java"),
            "package com.acme;\nimport com.acme.util.Helper;\nimport com.acme.util.*;\nimport java.util.List;\npublic class App {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/main/java/com/acme/util/Helper.java"),
            "package com.acme.util;\npublic class Helper {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/main/java/com/acme/util/Extra.java"),
            "package com.acme.util;\npublic class Extra {}\n",
        )
        .unwrap();

        let p = FallbackProvider::new(root, FbLang::Java);
        let g = p.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("src/main/java/com/acme/App.java")).expect("App.java edges");
        assert!(
            edges.contains(&PathBuf::from("src/main/java/com/acme/util/Helper.java")),
            "explicit import resolved under the maven source root: {edges:?}"
        );
        assert!(
            edges.contains(&PathBuf::from("src/main/java/com/acme/util/Extra.java")),
            "wildcard import pulled in the package's other file: {edges:?}"
        );
        assert!(
            !edges.iter().any(|e| e.to_string_lossy().contains("java/util")),
            "external dependency (java.util.List) produces NO edge: {edges:?}"
        );
    }

    // Java import resolution over a FLAT layout (no src/main/java): the package-decl-to-path
    // offset makes the repo root the source root, so `import com.x.B` still resolves.
    #[test]
    fn java_import_graph_flat_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("com/x")).unwrap();
        fs::write(
            root.join("com/x/A.java"),
            "package com.x;\nimport com.x.B;\npublic class A {}\n",
        )
        .unwrap();
        fs::write(root.join("com/x/B.java"), "package com.x;\npublic class B {}\n").unwrap();

        let p = FallbackProvider::new(root, FbLang::Java);
        let g = p.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("com/x/A.java")).expect("A.java edges");
        assert_eq!(edges, &vec![PathBuf::from("com/x/B.java")], "flat-layout import resolved via package offset: {edges:?}");
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
