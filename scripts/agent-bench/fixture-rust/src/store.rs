use crate::tokenize::tokenize;
use crate::types::{DocEntry, EntryKind};
use std::collections::HashMap;

/// The in-memory index: token -> doc ids, plus the doc table itself.
#[derive(Default)]
pub struct Store {
    pub docs: Vec<DocEntry>,
    postings: HashMap<String, Vec<usize>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index one document. The id is its position in the doc table.
    pub fn add(&mut self, name: &str, path: &str, kind: EntryKind, body: &str) -> usize {
        let id = self.docs.len();
        self.docs.push(DocEntry {
            name: name.to_string(),
            path: path.to_string(),
            score: 0.0,
            kind,
        });
        for token in tokenize(body) {
            self.postings.entry(token).or_default().push(id);
        }
        id
    }

    /// Doc ids whose body contained `token`, lexical hit count as the score.
    pub fn lookup(&self, token: &str) -> Vec<(String, f32)> {
        let Some(ids) = self.postings.get(token) else { return vec![] };
        let mut counts: HashMap<usize, f32> = HashMap::new();
        for id in ids {
            *counts.entry(*id).or_insert(0.0) += 1.0;
        }
        counts.into_iter().map(|(id, c)| (id.to_string(), c)).collect()
    }
}
