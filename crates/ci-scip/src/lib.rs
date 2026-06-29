//! ci-scip — read a SCIP index (`index.scip`) into the language-blind structure
//! tree + import graph. SCIP is block-per-named-symbol: each definition carries a
//! name range + an `enclosing_range` (the full span), and references across files
//! give a semantic dependency graph. This is the READ backbone for any language
//! whose indexer emits SCIP; `lang-ts` produces the index with `scip-typescript`.
//!
//! Note: `scip-typescript` leaves `SymbolInformation.kind`/`display_name` blank and
//! encodes everything in the **symbol moniker** (`…/add().` = method, `(a)` =
//! parameter, trailing `/` = file). So we parse the SCIP symbol grammar
//! ([`scip::symbol::parse_symbol`]) and read descriptor suffixes for name + kind.
use ci_core::{Error, ImportGraph, Node, NodeKind, Range, Result, SymbolKind};
use protobuf::Message;
use scip::symbol::parse_symbol;
use scip::types::{descriptor::Suffix, Document, Index, SymbolRole};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct ScipIndex {
    index: Index,
}

fn is_definition(roles: i32) -> bool {
    roles & (SymbolRole::Definition as i32) != 0
}

/// Map a descriptor suffix to a chunked symbol kind, or `None` if it's not a named
/// declaration we index (namespace, parameter, type-parameter, meta, local, macro).
fn descriptor_kind(s: Suffix) -> Option<SymbolKind> {
    match s {
        Suffix::Method => Some(SymbolKind::Function),
        Suffix::Type => Some(SymbolKind::Class), // SCIP `#` covers class/interface/enum/type
        Suffix::Term => Some(SymbolKind::Variable),
        _ => None,
    }
}

/// Parse a SCIP global symbol into (qualified name, leaf name, leaf kind), or `None`
/// if it isn't a chunked named declaration (parameter, file module, local, …).
/// `Foo#bar().` -> ("Foo.bar", "bar", Function); `add().(a)` (a parameter) -> None.
fn parse_named(symbol: &str) -> Option<(String, String, SymbolKind)> {
    let parsed = parse_symbol(symbol).ok()?;
    let leaf_kind = descriptor_kind(parsed.descriptors.last()?.suffix.enum_value_or_default())?;
    let parts: Vec<String> = parsed
        .descriptors
        .iter()
        .filter(|d| descriptor_kind(d.suffix.enum_value_or_default()).is_some())
        .map(|d| d.name.trim_matches('`').to_string())
        .collect();
    let leaf = parts.last()?.clone();
    Some((parts.join("."), leaf, leaf_kind))
}

/// SCIP range -> core Range. SCIP is 0-based `[startLine, startChar, endLine,
/// endChar]` (or `[line, startChar, endChar]` on one line); core lines are 1-based.
fn decode_range(r: &[i32]) -> Option<Range> {
    match r.len() {
        3 => Some(Range {
            start_line: (r[0] + 1) as u32,
            start_char: r[1] as u32,
            end_line: (r[0] + 1) as u32,
            end_char: r[2] as u32,
        }),
        4 => Some(Range {
            start_line: (r[0] + 1) as u32,
            start_char: r[1] as u32,
            end_line: (r[2] + 1) as u32,
            end_char: r[3] as u32,
        }),
        _ => None,
    }
}

impl ScipIndex {
    pub fn load(path: &Path) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let index =
            Index::parse_from_bytes(bytes).map_err(|e| Error::Other(format!("scip parse: {e}")))?;
        Ok(Self { index })
    }

    /// Repo-relative paths of every indexed document.
    pub fn documents(&self) -> Vec<String> {
        self.index.documents.iter().map(|d| d.relative_path.clone()).collect()
    }

    fn document(&self, rel: &str) -> Option<&Document> {
        self.index.documents.iter().find(|d| d.relative_path == rel)
    }

    /// Named-symbol structure tree for one file (shallow — SCIP granularity).
    pub fn structure(&self, rel: &str) -> Result<Vec<Node>> {
        let Some(doc) = self.document(rel) else { return Ok(vec![]) };
        let mut nodes = Vec::new();
        let mut id_counts: HashMap<String, u32> = HashMap::new();
        for occ in &doc.occurrences {
            if !is_definition(occ.symbol_roles) {
                continue;
            }
            let Some((qualified, leaf, kind)) = parse_named(&occ.symbol) else { continue };

            let name_range = decode_range(&occ.range);
            let full = decode_range(&occ.enclosing_range)
                .or_else(|| name_range.clone())
                .unwrap_or(Range { start_line: 0, end_line: 0, start_char: 0, end_char: 0 });

            // Stable id with ~N disambiguation on duplicate names (nodeId grammar).
            let base = format!("{rel}#{qualified}");
            let n = id_counts.entry(base.clone()).or_insert(0);
            let id = if *n == 0 { base.clone() } else { format!("{base}~{n}") };
            *n += 1;

            nodes.push(Node {
                id,
                name: Some(leaf),
                kind: NodeKind::Symbol(kind),
                range: full,
                name_range,
                children: vec![],
            });
        }
        Ok(nodes)
    }

    /// File-level import graph from semantic references: file A -> files defining
    /// symbols A references. More precise than parsing import statements.
    pub fn import_graph(&self) -> Result<ImportGraph> {
        let mut def_file: HashMap<&str, &str> = HashMap::new();
        for doc in &self.index.documents {
            for occ in &doc.occurrences {
                if is_definition(occ.symbol_roles) {
                    def_file.entry(occ.symbol.as_str()).or_insert(doc.relative_path.as_str());
                }
            }
        }

        let mut graph: ImportGraph = ImportGraph::new();
        for doc in &self.index.documents {
            let from = doc.relative_path.as_str();
            let mut targets: Vec<String> = Vec::new();
            for occ in &doc.occurrences {
                if is_definition(occ.symbol_roles) {
                    continue;
                }
                if let Some(&to) = def_file.get(occ.symbol.as_str()) {
                    if to != from && !targets.iter().any(|t| t == to) {
                        targets.push(to.to_string());
                    }
                }
            }
            if !targets.is_empty() {
                targets.sort();
                graph.insert(PathBuf::from(from), targets.into_iter().map(PathBuf::from).collect());
            }
        }
        Ok(graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scip::types::{Document, Index, Occurrence};

    fn def_occ(symbol: &str, name_range: Vec<i32>, enclosing: Vec<i32>) -> Occurrence {
        let mut o = Occurrence::new();
        o.symbol = symbol.into();
        o.symbol_roles = SymbolRole::Definition as i32;
        o.range = name_range;
        o.enclosing_range = enclosing;
        o
    }
    fn ref_occ(symbol: &str, range: Vec<i32>) -> Occurrence {
        let mut o = Occurrence::new();
        o.symbol = symbol.into();
        o.symbol_roles = 0;
        o.range = range;
        o
    }

    // Real SCIP monikers: `<scheme> <mgr> <pkg> <ver> <descriptors>`.
    const ADD: &str = "scip-typescript npm . . math/add().";
    const MAIN: &str = "scip-typescript npm . . app/main().";

    fn fixture() -> Vec<u8> {
        let mut math = Document::new();
        math.relative_path = "math.ts".into();
        math.occurrences = vec![def_occ(ADD, vec![0, 9, 0, 12], vec![0, 0, 2, 1])];

        let mut app = Document::new();
        app.relative_path = "app.ts".into();
        app.occurrences = vec![
            def_occ(MAIN, vec![0, 9, 0, 13], vec![0, 0, 4, 1]),
            ref_occ(ADD, vec![2, 2, 2, 5]),
        ];

        let mut index = Index::new();
        index.documents = vec![math, app];
        index.write_to_bytes().unwrap()
    }

    #[test]
    fn parse_named_reads_descriptors() {
        assert_eq!(
            parse_named(ADD),
            Some(("add".to_string(), "add".to_string(), SymbolKind::Function))
        );
        // a parameter symbol is not a chunked declaration
        assert_eq!(parse_named("scip-typescript npm . . math/add().(a)"), None);
        // the file/module symbol (namespace) is skipped
        assert_eq!(parse_named("scip-typescript npm . . math/"), None);
    }

    #[test]
    fn structure_reads_named_symbols_with_enclosing_range() {
        let scip = ScipIndex::from_bytes(&fixture()).unwrap();
        let nodes = scip.structure("math.ts").unwrap();
        assert_eq!(nodes.len(), 1);
        let n = &nodes[0];
        assert_eq!(n.id, "math.ts#add");
        assert_eq!(n.name.as_deref(), Some("add"));
        assert!(matches!(n.kind, NodeKind::Symbol(SymbolKind::Function)));
        assert_eq!(n.range.start_line, 1);
        assert_eq!(n.range.end_line, 3);
    }

    #[test]
    fn import_graph_from_references() {
        let scip = ScipIndex::from_bytes(&fixture()).unwrap();
        let g = scip.import_graph().unwrap();
        assert_eq!(g.get(&PathBuf::from("app.ts")).unwrap(), &vec![PathBuf::from("math.ts")]);
        assert!(g.get(&PathBuf::from("math.ts")).is_none());
    }

    #[test]
    fn documents_lists_all() {
        let scip = ScipIndex::from_bytes(&fixture()).unwrap();
        let mut docs = scip.documents();
        docs.sort();
        assert_eq!(docs, vec!["app.ts".to_string(), "math.ts".to_string()]);
    }
}
