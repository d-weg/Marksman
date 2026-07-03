use crate::dedupe::collapse_paths;
use crate::rank::blend_scores;
use crate::store::Store;
use crate::tokenize::tokenize;
use crate::types::{DocEntry, EntryKind};

/// Run a query: lexical lookups per token, blended with a (stub) semantic list,
/// deduped by path, top-N as fresh DocEntry hits carrying the fused score.
pub fn search(store: &Store, query: &str, top: usize) -> Vec<DocEntry> {
    let mut lexical: Vec<(String, f32)> = Vec::new();
    for token in tokenize(query) {
        lexical.extend(store.lookup(&token));
    }
    // Semantic ranking is a stub: the doc table order stands in for vector scores.
    let semantic: Vec<(String, f32)> =
        store.docs.iter().enumerate().map(|(i, d)| (i.to_string(), d.score)).collect();

    let fused = blend_scores(&lexical, &semantic);
    let mut hits: Vec<DocEntry> = Vec::new();
    for (id, score) in fused.into_iter().take(top * 2) {
        let Ok(idx) = id.parse::<usize>() else { continue };
        let Some(doc) = store.docs.get(idx) else { continue };
        hits.push(DocEntry {
            name: doc.name.clone(),
            path: doc.path.clone(),
            score,
            kind: EntryKind::Source,
        });
    }
    collapse_paths(hits).into_iter().take(top).collect()
}
