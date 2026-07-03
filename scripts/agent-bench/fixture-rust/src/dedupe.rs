use crate::types::DocEntry;
use std::collections::HashSet;

/// Keep the best-scoring hit per path (input must already be sorted best-first).
pub fn collapse_paths(hits: Vec<DocEntry>) -> Vec<DocEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    hits.into_iter().filter(|h| seen.insert(h.path.clone())).collect()
}
