//! Language-blind contract types. Drivers speak these; the core never sees syntax.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Symbol categories, normalized across languages. The TS driver maps ts-morph
/// node kinds onto these; future drivers map their own. Mirrors the nodeId
/// prefixes (fn_/cls_/iface_/enum_/type_/var_/meth_) plus doc sections.
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
    Doc,
}

/// 1-based line range (matches the TS manifest); char offsets are 0-based and
/// optional (drivers that only know lines may leave them 0).
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
    #[serde(rename = "RENAME")]
    Rename { node_id: String, new_name: String },
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
        matches!(back, EditOp::Rename { .. });
    }

    #[test]
    fn symbol_kind_serializes_protocol_names() {
        assert_eq!(serde_json::to_string(&SymbolKind::TypeAlias).unwrap(), "\"type\"");
        assert_eq!(serde_json::to_string(&SymbolKind::Variable).unwrap(), "\"var\"");
        assert_eq!(serde_json::to_string(&SymbolKind::Function).unwrap(), "\"function\"");
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
