//! BM25 lexical index — faithful port of src/bm25.ts (k1=1.5, b=0.75), including
//! the camelCase/snake_case-aware tokenizer that keeps the compound token too.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "for", "on", "is", "this", "that", "it", "as",
    "with", "by", "be", "are", "from", "at", "if", "we", "you", "return", "const",
];

fn is_stop(w: &str) -> bool {
    STOP.contains(&w)
}

/// Split a token on camelCase / PascalCase / digit boundaries, replicating the two
/// regexes in bm25.ts: `([a-z0-9])([A-Z])` and `([A-Z]+)([A-Z][a-z])`.
fn split_camel(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut s = String::with_capacity(word.len() + 4);
    for i in 0..chars.len() {
        let c = chars[i];
        if i > 0 {
            let prev = chars[i - 1];
            let r1 = (prev.is_ascii_lowercase() || prev.is_ascii_digit()) && c.is_ascii_uppercase();
            let r2 = prev.is_ascii_uppercase()
                && c.is_ascii_uppercase()
                && i + 1 < chars.len()
                && chars[i + 1].is_ascii_lowercase();
            if r1 || r2 {
                s.push(' ');
            }
        }
        s.push(c);
    }
    s.split(|ch: char| ch.is_whitespace() || ch == '_')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Tokenize code/text for lexical search (see bm25.ts `tokenize`).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
    {
        let lower = word.to_lowercase();
        if lower.chars().count() > 1 && !is_stop(&lower) {
            out.push(lower);
        }
        let parts = split_camel(word);
        if parts.len() > 1 {
            for p in parts {
                let lp = p.to_lowercase();
                if lp.chars().count() > 1 && !is_stop(&lp) {
                    out.push(lp);
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bm25Doc {
    pub id: String,
    pub file: String,
    pub len: usize,
    pub tf: HashMap<String, u32>,
}

/// On-disk form, matching bm25.ts toJSON/fromJSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bm25Json {
    pub k1: f64,
    pub b: f64,
    pub docs: Vec<Bm25Doc>,
    pub df: HashMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct Bm25 {
    pub k1: f64,
    pub b: f64,
    docs: HashMap<String, Bm25Doc>,
    df: HashMap<String, usize>,
}

impl Default for Bm25 {
    fn default() -> Self {
        Self { k1: 1.5, b: 0.75, docs: HashMap::new(), df: HashMap::new() }
    }
}

impl Bm25 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_json(j: Bm25Json) -> Self {
        let docs = j.docs.into_iter().map(|d| (d.id.clone(), d)).collect();
        Self { k1: j.k1, b: j.b, docs, df: j.df }
    }

    pub fn to_json(&self) -> Bm25Json {
        Bm25Json {
            k1: self.k1,
            b: self.b,
            docs: self.docs.values().cloned().collect(),
            df: self.df.clone(),
        }
    }

    pub fn add_doc(&mut self, id: &str, file: &str, tokens: &[String]) {
        if self.docs.contains_key(id) {
            self.remove_doc(id);
        }
        let mut tf: HashMap<String, u32> = HashMap::new();
        for t in tokens {
            *tf.entry(t.clone()).or_insert(0) += 1;
        }
        for t in tf.keys() {
            *self.df.entry(t.clone()).or_insert(0) += 1;
        }
        self.docs.insert(
            id.to_string(),
            Bm25Doc { id: id.to_string(), file: file.to_string(), len: tokens.len(), tf },
        );
    }

    pub fn remove_doc(&mut self, id: &str) {
        let Some(d) = self.docs.remove(id) else { return };
        for t in d.tf.keys() {
            if let Some(c) = self.df.get_mut(t) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    self.df.remove(t);
                }
            }
        }
    }

    pub fn remove_by_files(&mut self, files: &std::collections::HashSet<String>) {
        let ids: Vec<String> = self
            .docs
            .values()
            .filter(|d| files.contains(&d.file))
            .map(|d| d.id.clone())
            .collect();
        for id in ids {
            self.remove_doc(&id);
        }
    }

    fn avgdl(&self) -> f64 {
        if self.docs.is_empty() {
            return 0.0;
        }
        let total: usize = self.docs.values().map(|d| d.len).sum();
        total as f64 / self.docs.len() as f64
    }

    /// Ranked (id, score) for a tokenized query, descending. Mirrors bm25.ts.
    pub fn search(&self, query_tokens: &[String], top_k: usize) -> Vec<(String, f64)> {
        let n = self.docs.len();
        if n == 0 {
            return Vec::new();
        }
        let avgdl = self.avgdl();
        let mut qset: Vec<&String> = query_tokens.iter().collect();
        qset.sort();
        qset.dedup();

        let mut scored: Vec<(String, f64)> = Vec::new();
        for d in self.docs.values() {
            let mut score = 0.0;
            for &t in &qset {
                let Some(&f) = d.tf.get(t) else { continue };
                let f = f as f64;
                let n_t = *self.df.get(t).unwrap_or(&0) as f64;
                let idf = (1.0 + (n as f64 - n_t + 0.5) / (n_t + 0.5)).ln();
                let denom = f + self.k1 * (1.0 - self.b + self.b * d.len as f64 / avgdl.max(1.0));
                score += idf * (f * (self.k1 + 1.0) / denom);
            }
            if score > 0.0 {
                scored.push((d.id.clone(), score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_camel_keeps_compound() {
        let t = tokenize("buildIndex HTTPServer foo_bar");
        assert!(t.contains(&"buildindex".to_string()));
        assert!(t.contains(&"build".to_string()));
        assert!(t.contains(&"index".to_string()));
        assert!(t.contains(&"http".to_string()));
        assert!(t.contains(&"server".to_string()));
        assert!(t.contains(&"foo".to_string()));
        assert!(t.contains(&"bar".to_string()));
    }

    #[test]
    fn bm25_ranks_matching_doc_first() {
        let mut bm = Bm25::new();
        bm.add_doc("a", "a.ts", &tokenize("reciprocal rank fusion merge"));
        bm.add_doc("b", "b.ts", &tokenize("unrelated database migration"));
        let hits = bm.search(&tokenize("rank fusion"), 10);
        assert_eq!(hits[0].0, "a");
    }

    #[test]
    fn bm25_json_roundtrip() {
        let mut bm = Bm25::new();
        bm.add_doc("a", "a.ts", &tokenize("alpha beta gamma"));
        let j = bm.to_json();
        let bm2 = Bm25::from_json(j);
        assert_eq!(bm2.search(&tokenize("beta"), 5)[0].0, "a");
    }
}
