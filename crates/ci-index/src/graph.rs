//! Import graph: forward edges (file -> files it imports) come from the driver;
//! reverse edges are derived. Port of buildGraph/deriveReverse in import-graph.ts.
//! Language-blind — just string adjacency over repo-relative paths.
use std::collections::{BTreeMap, BTreeSet};

/// file -> list of files (repo-relative posix path strings).
pub type Adjacency = BTreeMap<String, Vec<String>>;

#[derive(Debug, Clone, Default)]
pub struct GraphData {
    pub forward: Adjacency,
    pub reverse: Adjacency,
}

pub fn derive_reverse(forward: &Adjacency) -> Adjacency {
    let mut rev: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (from, tos) in forward {
        for to in tos {
            rev.entry(to.clone()).or_default().insert(from.clone());
        }
    }
    rev.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect()
}

pub fn build_graph(forward: Adjacency) -> GraphData {
    let reverse = derive_reverse(&forward);
    GraphData { forward, reverse }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_is_derived() {
        let mut fwd = Adjacency::new();
        fwd.insert("a.ts".into(), vec!["b.ts".into(), "c.ts".into()]);
        fwd.insert("d.ts".into(), vec!["b.ts".into()]);
        let g = build_graph(fwd);
        assert_eq!(g.reverse.get("b.ts").unwrap(), &vec!["a.ts".to_string(), "d.ts".to_string()]);
        assert_eq!(g.reverse.get("c.ts").unwrap(), &vec!["a.ts".to_string()]);
    }
}
