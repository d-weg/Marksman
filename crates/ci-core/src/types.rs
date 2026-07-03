//! Language-blind contract types. Drivers speak these; the core never sees syntax.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Symbol categories, normalized across languages. The TS driver maps ts-morph
/// node kinds onto these; future drivers map their own. Mirrors the nodeId
/// prefixes (fn_/cls_/iface_/enum_/type_/var_/meth_).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Class,
    Interface,
    Enum,
    #[serde(rename = "type")]
    TypeAlias,
    #[serde(rename = "var")]
    Variable,
    Method,
    Struct,
}

impl SymbolKind {
    /// The lowercase protocol/display name — matches the serde representation (`type`/`var` for
    /// TypeAlias/Variable). Shared by the CLI and MCP surfaces so both render kinds identically.
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Enum => "enum",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Variable => "var",
            SymbolKind::Method => "method",
            SymbolKind::Struct => "struct",
        }
    }
}

/// 1-based line range (matches the TS manifest); column offsets are 0-based **UTF-8 byte**
/// offsets within the line (the tree-sitter / VFS convention — see [`crate::text::byte_offset`])
/// and optional (drivers that only know lines may leave them 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default)]
    pub start_char: u32,
    #[serde(default)]
    pub end_char: u32,
}

/// How deep a provider's structure tree goes. SCIP-backed providers are `Symbol`
/// (one block per named declaration); a future AST-backed provider is `Ast` (a node
/// per syntactic construct), which unlocks sub-symbol edits. Same trait, deeper tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    Symbol,
    Ast,
}

/// The kind of a structure node. Named declarations carry a [`SymbolKind`] and are
/// emitted by every provider; sub-symbol syntactic nodes (`"parameter"`,
/// `"returnType"`, `"body"`, …) carry a free-form tag and are AST-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeKind {
    Symbol(SymbolKind),
    Syntax(String),
}

/// A node in a language's structure tree. SCIP providers return a SHALLOW tree
/// (named declarations, with class→method nesting); an AST provider returns a DEEP
/// tree (params/return-type/body as children). The edit layer targets any node by
/// `id` + `range`, so deepening the tree later requires no core changes. Structure
/// only — the core reads file text from `range` for embedding/BM25.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    /// Stable address, e.g. `"math.ts#add"` or `"math.ts#MyClass.foo"`.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: NodeKind,
    /// Full editable span (SCIP `enclosing_range`).
    pub range: Range,
    /// The name/identifier span, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_range: Option<Range>,
    /// Sub-nodes. Empty for a leaf (a SCIP symbol block); populated by an AST provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
}

impl Node {
    /// Depth-first visit of this node and all descendants.
    pub fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Node)) {
        f(self);
        for c in &self.children {
            c.walk(f);
        }
    }

    /// The [`SymbolKind`] if this node is a named declaration, else `None`.
    pub fn symbol_kind(&self) -> Option<SymbolKind> {
        match &self.kind {
            NodeKind::Symbol(k) => Some(*k),
            NodeKind::Syntax(_) => None,
        }
    }
}

/// file (repo-relative) -> the files it imports (repo-relative).
pub type ImportGraph = BTreeMap<PathBuf, Vec<PathBuf>>;

/// Invert an [`ImportGraph`] (file → files it imports) into a reverse map (file → the files that
/// import it), keyed by repo-relative posix strings. Every tree-sitter/SCIP provider builds this
/// identically to feed the edit layer's delete-safety + blast-radius check (`reverse_imports`).
pub fn reverse_import_map(graph: &ImportGraph) -> HashMap<String, Vec<String>> {
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
    for (from, tos) in graph {
        let f = from.to_string_lossy().replace('\\', "/");
        for to in tos {
            reverse.entry(to.to_string_lossy().replace('\\', "/")).or_default().push(f.clone());
        }
    }
    reverse
}

/// The TRANSITIVE reverse-importer set of `file` (excluding `file` itself), BFS over a
/// [`reverse_import_map`]. A SYNTACTIC import graph does not flatten barrels — a consumer of
/// `export * from './x'` edges to the barrel, not to x — so the edit gate's one-hop blast
/// radius would miss it and could claim "clean" on a broken consumer (measured: bench task
/// T9-barrel). Providers whose graph is syntactic serve this closure instead; semantic (scip)
/// graphs are already flattened and keep the cheaper one-hop.
pub fn transitive_reverse_imports(reverse: &HashMap<String, Vec<String>>, file: &str) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut queue: Vec<&str> = vec![file];
    while let Some(f) = queue.pop() {
        for importer in reverse.get(f).map(|v| v.as_slice()).unwrap_or_default() {
            if importer != file && seen.insert(importer) {
                out.push(importer.clone());
                queue.push(importer);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Edit protocol — mirrors src/edit/types.ts `AgentOp` in the TS implementation.
// The blueprint's granular driver methods (set_body/rename/move/type_check) are
// folded into these ops + one atomic `apply_edits`, because the type-check gate
// must roll back across a whole multi-op edit — individual calls can't.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EditOp {
    #[serde(rename = "SET_BODY")]
    SetBody { node_id: String, body: String },
    #[serde(rename = "REPLACE_NODE")]
    ReplaceNode { node_id: String, code: String },
    #[serde(rename = "REPLACE_TEXT")]
    ReplaceText {
        node_id: String,
        old_text: String,
        new_text: String,
    },
    #[serde(rename = "INSERT_BEFORE")]
    InsertBefore { node_id: String, code: String },
    /// Insert `code` as a statement inside a function/method body (the `:body` sub-node). With
    /// `after` set, it lands on a new line after the (unique) line containing that text; otherwise
    /// it is appended at the end of the body. Statement-level — the block's other statements stay.
    #[serde(rename = "INSERT_IN_BODY")]
    InsertInBody {
        node_id: String,
        code: String,
        #[serde(default)]
        after: Option<String>,
    },
    /// Delete the statement line(s) containing `text` (unique within the body) from a `:body`.
    #[serde(rename = "DELETE_IN_BODY")]
    DeleteInBody { node_id: String, text: String },
    /// Insert `code` as a new member at the top of a `{ … }` block — an interface/type field, a
    /// class member, or an object-literal property. Targets the CONTAINER symbol directly (no
    /// `:body` sub-node needed); `code` must carry its own terminator (`;` for a type/interface
    /// member, `,` for an object property), since it lands ahead of the existing members.
    #[serde(rename = "INSERT_MEMBER")]
    InsertMember { node_id: String, code: String },
    /// Append a parameter to a function/method's parameter list (the `:params` sub-node), inserting
    /// before the closing `)` and prefixing `, ` when the list is non-empty.
    #[serde(rename = "ADD_PARAMETER")]
    AddParameter { node_id: String, param: String },
    /// Add a return type to a function/method that has none, at the language's insertion point
    /// (after `)`: TS `: T`, Rust/Python `-> T`). Refused if a return type already exists — use
    /// `replace_node target:return` for that.
    #[serde(rename = "SET_RETURN_TYPE")]
    SetReturnType { node_id: String, ty: String },
    #[serde(rename = "RENAME")]
    Rename { node_id: String, new_name: String },
    /// File-scoped text replacement: `old_text` must occur EXACTLY ONCE in `path`. The escape
    /// hatch for text outside every symbol anchor (imports, `mod` declarations, file-top
    /// statements) — same VFS + gate as every other op, no node addressing required.
    #[serde(rename = "REPLACE_IN_FILE")]
    ReplaceInFile { path: PathBuf, old_text: String, new_text: String },
    #[serde(rename = "MOVE_FILE")]
    MoveFile { from: PathBuf, to: PathBuf },
    #[serde(rename = "CREATE_FILE")]
    CreateFile { path: PathBuf, code: String },
    #[serde(rename = "DELETE_FILE")]
    DeleteFile { path: PathBuf },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditOpts {
    /// Persist on success; if false the gate runs but nothing is written.
    pub write: bool,
    /// Run the gate but never write, regardless of `write`.
    pub dry_run: bool,
    /// tsconfig path (driver-specific; ignored by drivers that don't need it).
    pub tsconfig: Option<String>,
}

/// Fresh read-path info for one file, produced by a write engine's live project right after a
/// committed edit (see `GateEngine::file_summaries`). For providers whose read index is a build
/// artifact (SCIP), this is how `structure()`/`import_graph()` stay true in-session: the same
/// compiler that gated the edit re-describes the changed files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSummary {
    /// Repo-relative posix path.
    pub path: String,
    /// True when the file no longer exists on disk (deleted, or the source of a move).
    pub deleted: bool,
    /// Shallow named-symbol nodes, same shape the SCIP read path produces (ids like
    /// `"file.ts#Class.method"`); any AST deepening stays the provider's job.
    pub nodes: Vec<Node>,
    /// Repo-relative files this file imports/re-exports (its outgoing graph edges).
    pub imports: Vec<PathBuf>,
}

/// A single diagnostic, keyed for baseline-diff (file + code + message, no line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diag {
    pub file: String,
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    pub ok: bool,
    #[serde(default)]
    pub diagnostics: Vec<Diag>,
}

/// Outcome of an atomic, gated edit batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum CommitResult {
    Ok {
        applied_ops: usize,
        changed_files: Vec<PathBuf>,
        #[serde(default)]
        repair_rounds: u32,
        /// PRE-EXISTING errors in the touched radius, excused by the baseline diff (clause 5:
        /// prior breakage never blocks an edit). Non-empty means the commit is legal but the
        /// radius is NOT clean — responses must say so instead of claiming "clean/COMPLETE"
        /// (bench move-rust round 4: a mid-flight repo stayed broken behind a COMPLETE claim).
        #[serde(default)]
        preexisting_in_radius: Vec<Diag>,
    },
    Rejected {
        #[serde(default)]
        failed_op_index: i64,
        feedback: String,
    },
}

// ---------------------------------------------------------------------------
// Retrieval manifest — mirrors src/types.ts `Manifest`.
// ---------------------------------------------------------------------------

/// A symbol the search surfaced inside a file (for the human summary).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchedSym {
    /// The self-locating node-id handle (`file#Scope.name`), so the agent can `read_node`/edit it
    /// directly instead of reconstructing it from name + file.
    #[serde(default)]
    pub node_id: String,
    pub name: String,
    pub kind: SymbolKind,
    /// [startLine, endLine], 1-based.
    pub line_range: [u32; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    pub file: String,
    pub pkg: String,
    pub matched_symbols: Vec<MatchedSym>,
    /// "query-match" | "imports-seed" | "imported-by-seed" | "doc".
    pub reason: String,
    pub score: f64,
    /// Present when the whole file is small enough to inline wholesale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whole_file: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedRank {
    pub file: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub task: String,
    pub generated_at: String,
    pub root: String,
    pub entries: Vec<ManifestEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seed_ranking: Vec<SeedRank>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editop_tag_roundtrip() {
        let op = EditOp::Rename {
            node_id: "src/a.ts#fn_alpha".into(),
            new_name: "beta".into(),
        };
        let j = serde_json::to_string(&op).unwrap();
        assert!(j.contains("\"type\":\"RENAME\""));
        let back: EditOp = serde_json::from_str(&j).unwrap();
        assert!(matches!(back, EditOp::Rename { .. }));
    }

    #[test]
    fn reverse_import_map_inverts_edges() {
        let mut g: ImportGraph = BTreeMap::new();
        g.insert(PathBuf::from("a.ts"), vec![PathBuf::from("c.ts")]);
        g.insert(PathBuf::from("b.ts"), vec![PathBuf::from("c.ts")]);
        let rev = reverse_import_map(&g);
        let mut importers = rev.get("c.ts").cloned().unwrap_or_default();
        importers.sort();
        assert_eq!(importers, vec!["a.ts".to_string(), "b.ts".to_string()]);
        assert!(!rev.contains_key("a.ts"), "a.ts has no importers");
    }

    // The barrel shape: app -> barrel -> policy. One-hop reverse of policy is just the barrel;
    // the transitive set must surface app (and never the file itself, even through a cycle).
    #[test]
    fn transitive_reverse_imports_crosses_barrels_and_survives_cycles() {
        let mut g: ImportGraph = BTreeMap::new();
        g.insert(PathBuf::from("app.ts"), vec![PathBuf::from("barrel.ts")]);
        g.insert(PathBuf::from("barrel.ts"), vec![PathBuf::from("policy.ts")]);
        g.insert(PathBuf::from("policy.ts"), vec![PathBuf::from("app.ts")]); // cycle back
        let rev = reverse_import_map(&g);

        let mut hops = transitive_reverse_imports(&rev, "policy.ts");
        hops.sort();
        assert_eq!(hops, vec!["app.ts".to_string(), "barrel.ts".to_string()]);
        assert!(!transitive_reverse_imports(&rev, "app.ts").contains(&"app.ts".to_string()));
    }

    #[test]
    fn symbol_kind_serializes_protocol_names() {
        assert_eq!(serde_json::to_string(&SymbolKind::TypeAlias).unwrap(), "\"type\"");
        assert_eq!(serde_json::to_string(&SymbolKind::Variable).unwrap(), "\"var\"");
        assert_eq!(serde_json::to_string(&SymbolKind::Function).unwrap(), "\"function\"");
    }

    #[test]
    fn as_str_matches_serde_name() {
        for k in [
            SymbolKind::Function, SymbolKind::Class, SymbolKind::Interface, SymbolKind::Enum,
            SymbolKind::TypeAlias, SymbolKind::Variable, SymbolKind::Method, SymbolKind::Struct,
        ] {
            let serde_name = serde_json::to_string(&k).unwrap();
            assert_eq!(format!("\"{}\"", k.as_str()), serde_name, "as_str vs serde for {k:?}");
        }
    }

    fn leaf(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.into(),
            name: None,
            kind,
            range: Range { start_line: 1, end_line: 3, start_char: 0, end_char: 0 },
            name_range: None,
            children: vec![],
        }
    }

    #[test]
    fn structure_tree_shallow_vs_deep() {
        // SCIP (shallow): a class block with method blocks, no sub-symbol children.
        let mut scip = leaf("a.ts#MyClass", NodeKind::Symbol(SymbolKind::Class));
        scip.children.push(leaf("a.ts#MyClass.foo", NodeKind::Symbol(SymbolKind::Method)));

        // AST (deep): the same method, but now with syntax children.
        let mut ast_method = leaf("a.ts#MyClass.foo", NodeKind::Symbol(SymbolKind::Method));
        ast_method.children.push(leaf("a.ts#MyClass.foo:param0", NodeKind::Syntax("parameter".into())));
        ast_method.children.push(leaf("a.ts#MyClass.foo:ret", NodeKind::Syntax("returnType".into())));

        // The core only chunks NAMED symbols, regardless of depth.
        let mut named = 0;
        scip.walk(&mut |n| if n.symbol_kind().is_some() { named += 1 });
        assert_eq!(named, 2); // MyClass + foo

        let mut syntax = 0;
        ast_method.walk(&mut |n| if matches!(n.kind, NodeKind::Syntax(_)) { syntax += 1 });
        assert_eq!(syntax, 2); // parameter + returnType (AST-only)
    }
}
