//! Config compatible with the TS tool's `codeindex.config.json` (camelCase keys),
//! merged over defaults so a partial file only overrides what it sets.
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryLayerWeighting {
    pub enabled: bool,
    pub boost: f32,
}

impl Default for QueryLayerWeighting {
    fn default() -> Self {
        Self { enabled: true, boost: 0.6 }
    }
}

fn default_symbol_match_bonus() -> f32 {
    3.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub embedding_model: String,
    pub top_n: usize,
    pub graph_hops: usize,
    pub max_expand: usize,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub languages: Vec<String>,
    pub index_docs: bool,
    pub doc_globs: Vec<String>,
    pub adjacency_bonus: f32,
    pub rrf_k: f32,
    /// Additive final-score bonus for a file whose symbol name the query matches, scaled by
    /// match strength (1.0 = the query contains the symbol's full name). Strong enough to lift
    /// a leaf *definition* above well-connected hub files that only win on adjacency — the
    /// definition of the thing you name is the answer for rename/locate tasks.
    #[serde(default = "default_symbol_match_bonus")]
    pub symbol_match_bonus: f32,
    pub index_dir: String,
    pub query_embed_prefix: String,
    #[serde(default)]
    pub query_layer_weighting: QueryLayerWeighting,
    #[serde(default)]
    pub package_weights: BTreeMap<String, f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            embedding_model: "Xenova/bge-small-en-v1.5".into(),
            top_n: 8,
            graph_hops: 2,
            max_expand: 10,
            include: vec!["**/*.ts".into(), "**/*.tsx".into()],
            exclude: vec![
                "**/node_modules/**".into(),
                "**/dist/**".into(),
                "**/build/**".into(),
                "**/*.d.ts".into(),
                "**/.codeindex/**".into(),
            ],
            languages: vec!["ts".into(), "tsx".into()],
            index_docs: true,
            doc_globs: vec!["**/*.md".into(), "**/*.mdx".into()],
            adjacency_bonus: 0.4,
            rrf_k: 60.0,
            symbol_match_bonus: default_symbol_match_bonus(),
            index_dir: ".codeindex".into(),
            query_embed_prefix: "Represent this sentence for searching relevant code: ".into(),
            query_layer_weighting: QueryLayerWeighting::default(),
            package_weights: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load `codeindex.config.json` (or `.codeindexrc.json`) merged over defaults.
    /// Missing file → defaults.
    pub fn load(root: &Path) -> Result<Config> {
        for name in ["codeindex.config.json", ".codeindexrc.json"] {
            let p = root.join(name);
            if let Ok(raw) = std::fs::read_to_string(&p) {
                let over: serde_json::Value = serde_json::from_str(&raw)?;
                let mut base = serde_json::to_value(Config::default())?;
                merge(&mut base, &over);
                return Ok(serde_json::from_value(base)?);
            }
        }
        Ok(Config::default())
    }
}

/// Deep-merge `over` onto `base` (objects recurse; everything else overwrites).
fn merge(base: &mut serde_json::Value, over: &serde_json::Value) {
    if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            match b.get_mut(k) {
                Some(bv) if bv.is_object() && v.is_object() => merge(bv, v),
                _ => {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_ts_includes() {
        let c = Config::default();
        assert!(c.include.iter().any(|g| g.contains("*.ts")));
        assert_eq!(c.rrf_k, 60.0);
    }

    #[test]
    fn partial_overrides_merge_over_defaults() {
        // Simulate a partial camelCase config the way load() does.
        let over: serde_json::Value =
            serde_json::from_str(r#"{ "embeddingModel": "minishlab/potion-code-16M", "topN": 12 }"#)
                .unwrap();
        let mut base = serde_json::to_value(Config::default()).unwrap();
        merge(&mut base, &over);
        let c: Config = serde_json::from_value(base).unwrap();
        assert_eq!(c.embedding_model, "minishlab/potion-code-16M");
        assert_eq!(c.top_n, 12);
        // untouched default preserved
        assert_eq!(c.graph_hops, 2);
    }
}
