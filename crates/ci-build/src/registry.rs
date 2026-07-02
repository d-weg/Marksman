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

    /// Index of the entry handling `file`'s language — a stable provider identity, so a caller
    /// can GROUP a mixed-language batch per provider (two langs may share one provider).
    pub fn entry_for(&self, file: &Path) -> Option<usize> {
        let lang = Lang::of(file);
        self.entries.iter().position(|(langs, _)| langs.contains(&lang))
    }

    /// The provider at `entry_for`'s index.
    pub fn entry_at(&self, i: usize) -> Option<&dyn LanguageProvider> {
        self.entries.get(i).map(|(_, p)| p.as_ref())
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

/// The outcome of [`build_registry`]: the per-file dispatch registry plus the languages that were
/// *expected* (present in the repo AND enabled) yet whose provider failed to construct — a `None`
/// from `make`, e.g. scip-typescript couldn't start. The registry is INCOMPLETE for those: files
/// in a failed language have no provider, so live reads/edits silently no-op on them. Callers that
/// serve edits must NOT cache an incomplete registry (a transient toolchain failure would be baked
/// in for the process's whole life) — inspect `failed` and retry/surface instead. The index-time
/// caller can proceed with a partial index (better than none) and just warns.
pub struct RegistryBuild {
    pub registry: ProviderRegistry,
    pub failed: Vec<&'static str>,
}

/// Build the provider registry for `root` with per-file dispatch. Detects which languages actually
/// have source files, honors the manifest (`config.providers.<lang>.enabled`), forces one language
/// when `CI_LANG` is set, rewrites `config.include`/`exclude` to cover exactly the active
/// languages, and asks `make` to construct each active language's provider. A `None` from `make`
/// (e.g. TS indexing failed) drops that language from the registry rather than failing the whole
/// build, but records it in [`RegistryBuild::failed`] so an edit-serving caller can tell a genuine
/// absence apart from a toolchain that didn't come up. Absent or disabled languages register
/// nothing — so their tooling is never fetched or run, and they are NOT reported as failed.
pub fn build_registry(
    root: &Path,
    config: &mut Config,
    mut make: impl FnMut(&str) -> Option<Arc<dyn LanguageProvider>>,
) -> Result<RegistryBuild> {
    let forced = forced_lang();
    let present = present_langs(root, config)?;

    let mut registry = ProviderRegistry::new();
    let mut failed: Vec<&'static str> = Vec::new();
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
        match make(spec.name) {
            Some(provider) => {
                registry.register(spec.langs.to_vec(), provider);
                includes.extend(spec.globs.iter().map(|g| g.to_string()));
                excludes.extend(spec.excludes.iter().map(|g| g.to_string()));
            }
            // Active + enabled but the toolchain didn't come up: an incomplete registry, not a
            // legitimate absence. Record it so edit-serving callers refuse to cache this build.
            None => failed.push(spec.name),
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
    Ok(RegistryBuild { registry, failed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_core::{Granularity, ImportGraph, Result};

    /// A do-nothing provider — build_registry only registers/queries by language, never calls in.
    struct StubProvider;
    impl LanguageProvider for StubProvider {
        fn granularity(&self) -> Granularity {
            Granularity::Symbol
        }
        fn structure(&self, _file: &Path) -> Result<Vec<ci_core::Node>> {
            Ok(vec![])
        }
        fn import_graph(&self) -> Result<ImportGraph> {
            Ok(ImportGraph::default())
        }
        fn apply_edits(&self, _ops: &[ci_core::EditOp], _opts: &ci_core::EditOpts) -> Result<ci_core::CommitResult> {
            unimplemented!()
        }
    }

    /// A repo with both a `.ts` and a `.rs` file, so TS and Rust are both "present".
    fn mixed_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.ts"), "export function f() {}\n").unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "pub fn g() {}\n").unwrap();
        dir
    }

    /// A present + enabled language whose provider fails to construct (make → None) is reported in
    /// `failed` AND dropped from the registry — the signal an edit-serving caller needs to refuse to
    /// cache a degraded build, instead of silently serving files with no provider.
    #[test]
    fn failed_provider_is_reported_not_silently_dropped() {
        let dir = mixed_repo();
        let mut config = Config::default();
        // Rust comes up; TS "toolchain" fails.
        let built = build_registry(dir.path(), &mut config, |lang| match lang {
            "rust" => Some(Arc::new(StubProvider) as Arc<dyn LanguageProvider>),
            _ => None,
        })
        .unwrap();
        assert_eq!(built.failed, vec!["ts"], "a present+enabled lang that failed to build must be reported");
        assert!(built.registry.provider_for(Path::new("src/a.ts")).is_none(), "no TS provider registered");
        assert!(built.registry.provider_for(Path::new("src/b.rs")).is_some(), "Rust provider registered");
    }

    /// When every present language's provider constructs, `failed` is empty — the caller caches.
    #[test]
    fn all_providers_up_reports_no_failure() {
        let dir = mixed_repo();
        let mut config = Config::default();
        let built = build_registry(dir.path(), &mut config, |_lang| {
            Some(Arc::new(StubProvider) as Arc<dyn LanguageProvider>)
        })
        .unwrap();
        assert!(built.failed.is_empty(), "no failures expected, got {:?}", built.failed);
        assert!(built.registry.provider_for(Path::new("src/a.ts")).is_some());
        assert!(built.registry.provider_for(Path::new("src/b.rs")).is_some());
    }

    /// A language that's absent (or disabled) is NOT a failure — only present+enabled langs whose
    /// toolchain didn't come up are reported, so a Rust-only repo never "fails" on missing TS.
    #[test]
    fn absent_language_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/only.rs"), "pub fn g() {}\n").unwrap();
        let mut config = Config::default();
        let built = build_registry(dir.path(), &mut config, |lang| match lang {
            "rust" => Some(Arc::new(StubProvider) as Arc<dyn LanguageProvider>),
            _ => None, // TS/Python would fail — but they're absent, so make is never called for them
        })
        .unwrap();
        assert!(built.failed.is_empty(), "absent languages must not be reported as failed");
    }
}
