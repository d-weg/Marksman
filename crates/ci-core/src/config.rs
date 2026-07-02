//! Config (`marksman.config.json`, camelCase keys — the legacy `codeindex.config.json`
//! names are still read),
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

/// One language's entry in the provider manifest (`providers.<lang>` in the config). Lets a repo
/// turn a language off, pin the tool version, or point at a vendored binary (offline/air-gapped).
/// Every field is optional so a partial entry (`{ "enabled": false }`) only overrides what it sets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderManifest {
    /// Whether this language is indexed/edited at all. `None` = default (enabled); `Some(false)`
    /// gates the language out of the provider registry so its tooling never runs.
    pub enabled: Option<bool>,
    /// Pin the language tool's version (e.g. the `scip-typescript` npm version). Advisory — passed
    /// through to the tool invocation where it supports a version selector.
    pub version: Option<String>,
    /// Point at a vendored tool binary (the sidecar process / language server) instead of resolving
    /// one from `PATH` — the offline/air-gapped path.
    pub bin: Option<String>,
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
    /// Per-language toggle for the SCIP-backed graph: `{ "scip": { "rust": true } }`. Where SCIP is
    /// *optional* this turns it on — today that's Rust (`rust-analyzer scip` `use` edges vs the
    /// `mod`-only tree-sitter graph), which costs ≈ a `cargo check` at index time, so it's off by
    /// default. (TS always uses SCIP; other languages are future hooks.) `CI_SCIP_<LANG>` (e.g.
    /// `CI_SCIP_RUST`) overrides a language per-run.
    #[serde(default)]
    pub scip: BTreeMap<String, bool>,
    /// Per-language provider manifest: enable/disable a language, pin a tool version, point at a
    /// vendored binary. Keyed by language name (`ts` / `rust` / `python`). Absent languages take
    /// the defaults (enabled, tools resolved from `PATH`/env).
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderManifest>,
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
                "**/.marksman/**".into(),
            ],
            languages: vec!["ts".into(), "tsx".into()],
            adjacency_bonus: 0.4,
            rrf_k: 60.0,
            symbol_match_bonus: default_symbol_match_bonus(),
            index_dir: ".marksman".into(),
            query_embed_prefix: "Represent this sentence for searching relevant code: ".into(),
            query_layer_weighting: QueryLayerWeighting::default(),
            package_weights: BTreeMap::new(),
            scip: BTreeMap::new(),
            providers: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load `marksman.config.json` (or the legacy `codeindex.config.json`/`.codeindexrc.json`)
    /// merged over defaults.
    /// Missing file → defaults.
    pub fn load(root: &Path) -> Result<Config> {
        for name in ["marksman.config.json", "codeindex.config.json", ".codeindexrc.json"] {
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

    /// Whether SCIP is enabled for `lang`: the `scip.<lang>` config setting, overridden by the
    /// `CI_SCIP_<LANG>` env var when present (`0`/`false`/empty = off, anything else = on). The
    /// legacy `CI_RUST_SCIP` is still honored for `rust`.
    pub fn scip_enabled(&self, lang: &str) -> bool {
        let on = |v: String| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false");
        if let Ok(v) = std::env::var(format!("CI_SCIP_{}", lang.to_uppercase())) {
            return on(v);
        }
        if lang == "rust" {
            if let Ok(v) = std::env::var("CI_RUST_SCIP") {
                return on(v);
            }
        }
        self.scip.get(lang).copied().unwrap_or(false)
    }

    /// Whether `lang`'s provider is enabled (the manifest's `enabled`, defaulting to `true`), with
    /// a `CI_PROVIDER_<LANG>_ENABLED` env override (`0`/`false`/empty = off). A disabled language is
    /// left out of the provider registry entirely, so its tooling never runs.
    pub fn provider_enabled(&self, lang: &str) -> bool {
        if let Ok(v) = std::env::var(format!("CI_PROVIDER_{}_ENABLED", lang.to_uppercase())) {
            return !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false");
        }
        self.providers.get(lang).and_then(|p| p.enabled).unwrap_or(true)
    }

    /// The vendored tool binary for `lang` from the manifest, if any (offline/air-gapped path).
    pub fn provider_bin(&self, lang: &str) -> Option<&str> {
        self.providers.get(lang).and_then(|p| p.bin.as_deref())
    }

    /// The pinned tool version for `lang` from the manifest, if any.
    pub fn provider_version(&self, lang: &str) -> Option<&str> {
        self.providers.get(lang).and_then(|p| p.version.as_deref())
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
    fn scip_map_parses_and_resolves_per_language() {
        let over: serde_json::Value = serde_json::from_str(r#"{ "scip": { "rust": true } }"#).unwrap();
        let mut base = serde_json::to_value(Config::default()).unwrap();
        merge(&mut base, &over);
        let c: Config = serde_json::from_value(base).unwrap();
        assert_eq!(c.scip.get("rust"), Some(&true), "scip.rust parsed");
        assert!(c.scip_enabled("rust"), "rust enabled via config");
        assert!(!c.scip_enabled("go"), "unset language off");
        assert!(Config::default().scip.is_empty(), "empty by default");
    }

    #[test]
    fn provider_manifest_parses_and_gates() {
        let over: serde_json::Value = serde_json::from_str(
            r#"{ "providers": { "python": { "enabled": false }, "ts": { "version": "0.3.14", "bin": "/opt/vendored/marksman-provider-ts" } } }"#,
        )
        .unwrap();
        let mut base = serde_json::to_value(Config::default()).unwrap();
        merge(&mut base, &over);
        let c: Config = serde_json::from_value(base).unwrap();
        // enabled defaults to true for unlisted/omitted languages, false only when set so.
        assert!(!c.provider_enabled("python"), "python disabled via manifest");
        assert!(c.provider_enabled("ts"), "ts has no enabled key → default true");
        assert!(c.provider_enabled("rust"), "unlisted language → default true");
        // version + vendored binary are surfaced for the tool-resolution seam.
        assert_eq!(c.provider_version("ts"), Some("0.3.14"));
        assert_eq!(c.provider_bin("ts"), Some("/opt/vendored/marksman-provider-ts"));
        assert_eq!(c.provider_bin("rust"), None);
        assert!(Config::default().providers.is_empty(), "empty by default");
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
