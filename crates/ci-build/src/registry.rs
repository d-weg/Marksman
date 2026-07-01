//! The extension → provider registry: the seam that lets a mixed-language repo index fully.
//!
//! Retrieval is language-blind (one unified BM25 + vector index), so multi-language support is
//! entirely an *index-time* concern: pick the right [`LanguageProvider`] for each file by its
//! extension. This module owns that dispatch and the lazy construction that only spins up a
//! language's tooling when the repo actually contains its files (Node only for a `.ts*` repo).
use ci_core::{Config, LanguageProvider, Node, Result};
use ci_walk::{present_langs, Lang};
use std::path::Path;
use std::sync::Arc;

/// Maps each source language to the provider that indexes/edits it, so a file dispatches to the
/// right provider by extension. Language-blind: it only knows `Lang → provider`; the caller
/// registers whichever concrete providers the repo needs. Cheap to clone (each provider is an
/// `Arc`), so the MCP hands one out of its lock per call.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    entries: Vec<(Vec<Lang>, Arc<dyn LanguageProvider>)>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` as the handler for every language in `langs`.
    pub fn register(&mut self, langs: Vec<Lang>, provider: Arc<dyn LanguageProvider>) -> &mut Self {
        self.entries.push((langs, provider));
        self
    }

    /// A registry with a single provider serving every code language — the trivial one-provider
    /// case (and what the unit tests use).
    pub fn single(provider: Arc<dyn LanguageProvider>) -> Self {
        let mut r = Self::new();
        r.register(vec![Lang::Ts, Lang::Tsx, Lang::Rust, Lang::Python], provider);
        r
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The provider that handles `file`'s language, if one is registered.
    pub fn provider_for(&self, file: &Path) -> Option<&dyn LanguageProvider> {
        let lang = Lang::of(file);
        self.entries.iter().find(|(langs, _)| langs.contains(&lang)).map(|(_, p)| p.as_ref())
    }

    /// Every registered provider (for the union import graph and prewarm).
    pub fn providers(&self) -> impl Iterator<Item = &dyn LanguageProvider> {
        self.entries.iter().map(|(_, p)| p.as_ref())
    }

    /// Structure for one file via its language's provider; empty when no provider handles it (a
    /// file whose language isn't registered — e.g. it was disabled in the manifest).
    pub fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        match self.provider_for(file) {
            Some(p) => p.structure(file),
            None => Ok(vec![]),
        }
    }

    /// Warm every provider's write engine (see [`LanguageProvider::prewarm`]).
    pub fn prewarm_all(&self) {
        for p in self.providers() {
            p.prewarm();
        }
    }
}

/// A supported source language: its manifest name, the include globs that select its files, the
/// `Lang` tags it owns, and any extra exclude globs (build-output dirs). The single source of
/// truth for `name ↔ extensions`, shared by the CLI and MCP registry builders.
struct LangSpec {
    name: &'static str,
    globs: &'static [&'static str],
    langs: &'static [Lang],
    excludes: &'static [&'static str],
}

const SUPPORTED: &[LangSpec] = &[
    LangSpec {
        name: "ts",
        globs: &["**/*.ts", "**/*.tsx", "**/*.mts", "**/*.cts"],
        langs: &[Lang::Ts, Lang::Tsx],
        excludes: &[],
    },
    LangSpec {
        name: "rust",
        globs: &["**/*.rs"],
        langs: &[Lang::Rust],
        excludes: &["**/target/**"],
    },
    LangSpec {
        name: "python",
        globs: &["**/*.py", "**/*.pyi"],
        langs: &[Lang::Python],
        excludes: &[],
    },
];

/// The single language `CI_LANG` forces, if set (`rust` / `ts`|`typescript` / `python`|`py`).
fn forced_lang() -> Option<&'static str> {
    match std::env::var("CI_LANG").ok().as_deref() {
        Some("rust") => Some("rust"),
        Some("ts") | Some("typescript") => Some("ts"),
        Some("python") | Some("py") => Some("python"),
        _ => None,
    }
}

/// Build the provider registry for `root` with per-file dispatch. Detects which languages actually
/// have source files, honors the manifest (`config.providers.<lang>.enabled`), forces one language
/// when `CI_LANG` is set, rewrites `config.include`/`exclude` to cover exactly the active
/// languages, and asks `make` to construct each active language's provider. A `None` from `make`
/// (e.g. TS indexing failed) drops that language rather than failing the whole build. Absent or
/// disabled languages register nothing — so their tooling is never fetched or run.
pub fn build_registry(
    root: &Path,
    config: &mut Config,
    mut make: impl FnMut(&str) -> Option<Arc<dyn LanguageProvider>>,
) -> Result<ProviderRegistry> {
    let forced = forced_lang();
    let present = present_langs(root, config)?;

    let mut registry = ProviderRegistry::new();
    let mut includes: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();

    for spec in SUPPORTED {
        if let Some(f) = forced {
            if spec.name != f {
                continue;
            }
        }
        // A forced language is tried even on a repo with none of its files (matching the old
        // single-provider escape hatch); otherwise a language activates only when present.
        let active = forced == Some(spec.name) || spec.langs.iter().any(|l| present.contains(l));
        if !active || !config.provider_enabled(spec.name) {
            continue;
        }
        if let Some(provider) = make(spec.name) {
            registry.register(spec.langs.to_vec(), provider);
            includes.extend(spec.globs.iter().map(|g| g.to_string()));
            excludes.extend(spec.excludes.iter().map(|g| g.to_string()));
        }
    }

    // Restrict the walk to the active languages' files (leave the defaults if nothing activated,
    // so a docs-only repo still discovers its markdown).
    if !includes.is_empty() {
        config.include = includes;
    }
    for e in excludes {
        if !config.exclude.contains(&e) {
            config.exclude.push(e);
        }
    }
    Ok(registry)
}
