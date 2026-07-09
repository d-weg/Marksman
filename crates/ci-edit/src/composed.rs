// ── Composed: ReadIndex × GateEngine = LanguageProvider ─────────────────────────────────────

use crate::{commit_edits, spawn_prewarm, GateEngine};
use ci_core::{CommitResult, EditOp, EditOpts, Error, FileSummary, Node, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Builds the write engine on first use (lazily / off-thread via `prewarm`).
pub type EngineFactory = std::sync::Arc<dyn Fn(&Path) -> Result<Box<dyn GateEngine + Send>> + Send + Sync>;

/// A [`LanguageProvider`] assembled from its two halves: a [`ReadIndex`] (the artifact or
/// live parser the agent PLANS against) and a [`GateEngine`] (the checker its edits run
/// through). The halves talk over exactly three channels, and the wiring POLICY is derived
/// from the reader's advertised properties instead of hand-wired per language:
///
/// 1. **radius** (read -> engine): the reverse-import set fed to [`commit_edits`] — one hop
///    when [`ReadIndex::semantic_edges`] (compiler-accurate graphs flatten barrels),
///    transitive otherwise (bench T9: a syntactic one-hop radius lets a barrel hide its
///    consumers).
/// 2. **freshness** (engine -> read): after a committed edit, artifact readers get overrides
///    from `GateEngine::file_summaries` so reads track the commit until the next reindex;
///    [`ReadIndex::live`] readers skip this — they re-parse current disk by construction.
///    Each read consults the overrides only where it is artifact-backed: structure by
///    `live()`, the import graph by [`ReadIndex::live_graph`] (a hybrid reader keeps live
///    structure while its cached graph stays overlay-corrected). A reader that derives
///    read-time structure from current disk registers a [`FreshDeepener`] so overrides are
///    served at the same depth as its artifact reads.
/// 3. **anchors**: edit ops resolve against the read structure the agent actually saw.
use ci_core::{Granularity, ImportGraph, LanguageProvider, ReadIndex};
use std::sync::{Arc, Mutex};

/// Live re-description of one repo-relative file (current-disk symbols + imports) for
/// artifact readers whose ENGINE can't provide `file_summaries` — e.g. a tree-sitter parse.
pub type LiveSummarizer = Arc<dyn Fn(&str) -> Option<FileSummary> + Send + Sync>;

/// Read-time enrichment of structure served FROM A FRESH SUMMARY (repo-relative path,
/// summary nodes -> served nodes). An artifact reader that derives extra structure from
/// current disk on every read (lang-ts: tree-sitter re-anchor + sub-node deepening over the
/// scip symbols) registers that same step here, so a fresh override is served at the same
/// depth as an artifact read — without it, post-commit reads would lose the derived
/// sub-node anchors (`:body`/`:params`/…) until the next reindex. Irrelevant for `live()`
/// readers: their structure never comes from the fresh map.
pub type FreshDeepener = Arc<dyn Fn(&str, Vec<Node>) -> Vec<Node> + Send + Sync>;

/// Builds an ALREADY-WARM engine for [`LanguageProvider::prewarm`]. The warming call is
/// per-toolchain (rust-analyzer needs a real file's diagnostics to force the cargo workspace
/// load — an empty pull would route through the cargo gate instead of the LSP); the default,
/// engine factory + an empty `diagnostics` call, fits bare LSP engines. Returns `None` when
/// the engine can't start: the slot stays empty and first use starts one lazily, surfacing
/// the error there.
pub type Prewarmer = Arc<dyn Fn(&Path) -> Option<Box<dyn GateEngine + Send>> + Send + Sync>;

pub struct Composed<R: ReadIndex> {
    root: PathBuf,
    read: R,
    engine_factory: EngineFactory,
    engine: Arc<Mutex<Option<Box<dyn GateEngine + Send>>>>,
    fresh: Arc<Mutex<HashMap<String, FileSummary>>>,
    live_summarizer: Option<LiveSummarizer>,
    fresh_deepener: Option<FreshDeepener>,
    prewarmer: Option<Prewarmer>,
}

impl<R: ReadIndex> Composed<R> {
    pub fn new(root: &Path, read: R, engine_factory: EngineFactory) -> Self {
        Self {
            root: root.to_path_buf(),
            read,
            engine_factory,
            engine: Arc::new(Mutex::new(None)),
            fresh: Arc::new(Mutex::new(HashMap::new())),
            live_summarizer: None,
            fresh_deepener: None,
            prewarmer: None,
        }
    }

    /// The freshness fallback for artifact readers: when the engine returns no
    /// `file_summaries` (LSP engines), re-describe committed files with this instead of
    /// letting reads lag until the next reindex. Irrelevant for `live()` readers.
    pub fn with_live_summarizer(mut self, s: LiveSummarizer) -> Self {
        self.live_summarizer = Some(s);
        self
    }

    /// Register the reader's read-time structure enrichment (see [`FreshDeepener`]) so
    /// nodes served from a fresh summary get the same treatment as an artifact read.
    pub fn with_fresh_deepener(mut self, d: FreshDeepener) -> Self {
        self.fresh_deepener = Some(d);
        self
    }

    /// Replace the default prewarm recipe (factory + empty diagnostics) with the
    /// language's own [`Prewarmer`].
    pub fn with_prewarmer(mut self, p: Prewarmer) -> Self {
        self.prewarmer = Some(p);
        self
    }

}

impl<R: ReadIndex> LanguageProvider for Composed<R> {
    fn granularity(&self) -> Granularity {
        self.read.granularity()
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        if !self.read.live() {
            if let Ok(m) = self.fresh.lock() {
                // Repo-relative posix key — callers pass relative OR absolute paths; the
                // fresh map must not miss an override because of the spelling.
                let rel = ci_core::rel_path(&self.root, file);
                if let Some(s) = m.get(&rel) {
                    if s.deleted {
                        return Ok(vec![]);
                    }
                    return Ok(match &self.fresh_deepener {
                        Some(deepen) => deepen(&rel, s.nodes.clone()),
                        None => s.nodes.clone(),
                    });
                }
            }
        }
        self.read.structure(file)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        let mut g = self.read.import_graph()?;
        if !self.read.live_graph() {
            if let Ok(m) = self.fresh.lock() {
                for s in m.values() {
                    let key = PathBuf::from(&s.path);
                    if s.deleted || s.imports.is_empty() {
                        g.remove(&key);
                    } else {
                        g.insert(key, s.imports.clone());
                    }
                }
            }
        }
        Ok(g)
    }

    fn prewarm(&self) {
        let root = self.root.clone();
        let make: Box<dyn FnOnce() -> Option<Box<dyn GateEngine + Send>> + Send> =
            match &self.prewarmer {
                Some(p) => {
                    let p = p.clone();
                    Box::new(move || p(&root))
                }
                None => {
                    let factory = self.engine_factory.clone();
                    Box::new(move || {
                        let mut engine = factory(&root).ok()?;
                        let _ = engine.diagnostics(&[]);
                        Some(engine)
                    })
                }
            };
        spawn_prewarm(self.engine.clone(), make);
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        // CI_TIMING=1: per-phase wall times on stderr (profiling only, no behavior change).
        let timing = std::env::var("CI_TIMING").is_ok();
        let t0 = std::time::Instant::now();
        let mut guard = self.engine.lock().map_err(|_| Error::Driver("engine lock poisoned".into()))?;
        if guard.is_none() {
            *guard = Some((self.engine_factory)(&self.root)?);
        }
        let engine: &mut dyn GateEngine = guard.as_mut().unwrap().as_mut();
        if timing {
            eprintln!("[timing] engine ready (warm or fresh) {:?}", t0.elapsed());
        }
        let t1 = std::time::Instant::now();

        let structure_of = |f: &str| self.structure(Path::new(f)).unwrap_or_default();
        // Channel 1 — radius policy from the reader's edge semantics.
        let reverse = ci_core::reverse_import_map(&self.import_graph().unwrap_or_default());
        let semantic = self.read.semantic_edges();
        let reverse_imports = |file: &str| {
            if semantic {
                reverse.get(file).cloned().unwrap_or_default()
            } else {
                ci_core::transitive_reverse_imports(&reverse, file)
            }
        };
        let r = commit_edits(&self.root, ops, &structure_of, engine, opts, &reverse_imports);
        if timing {
            eprintln!("[timing] commit_edits (warmup+rename+gate) {:?}", t1.elapsed());
        }

        // Channel 2 — freshness push-back, artifact readers only (best-effort: a refresh
        // hiccup must never fail an already-committed edit; reads then lag until reindex).
        // A hybrid reader (live structure, artifact graph) gets the push too — its
        // `import_graph` overlay is what keeps the artifact edges true in-session.
        if !self.read.live() || !self.read.live_graph() {
            if let Ok(CommitResult::Ok { changed_files, .. }) = &r {
                if opts.write && !opts.dry_run && !changed_files.is_empty() {
                    let rels: Vec<String> =
                        changed_files.iter().map(|p| ci_core::rel_path(&self.root, p)).collect();
                    match engine.file_summaries(&rels) {
                        Ok(Some(summaries)) => {
                            if let Ok(mut m) = self.fresh.lock() {
                                for s in summaries {
                                    m.insert(s.path.clone(), s);
                                }
                            }
                        }
                        Ok(None) => {
                            // Engine can't re-describe (LSP engines): use the recipe's live
                            // summarizer if it has one; else reads lag until the next reindex.
                            if let Some(summarize) = &self.live_summarizer {
                                if let Ok(mut m) = self.fresh.lock() {
                                    for rel in &rels {
                                        if let Some(s) = summarize(rel) {
                                            m.insert(rel.clone(), s);
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => eprintln!("[composed] post-edit read refresh failed ({e}); reads lag until reindex"),
                    }
                }
            }
        }
        // Reject-recovery: a gate that returns Err may have left its engine wedged — a resident
        // sidecar whose child died (lang-java's javax.tools process) stays cached and every later
        // edit would call the same dead process forever (its own error even promises "restart the
        // edit to respawn it"). Drop the cached engine so the NEXT apply_edits rebuilds it. Engines
        // that spawn per-call (PHP/Swift) rebuild identically, so this is safe for all providers.
        if r.is_err() {
            *guard = None;
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::fn_node;
    use ci_core::{Diag, Granularity};
    use serde_json::{json, Value};

    /// An artifact-or-hybrid reader with a fixed base graph and fixed structure nodes.
    struct StubReader {
        graph: ImportGraph,
        nodes: Vec<Node>,
        live: bool,
        live_graph: bool,
    }

    impl ReadIndex for StubReader {
        fn granularity(&self) -> Granularity {
            Granularity::Ast
        }
        fn structure(&self, _file: &Path) -> Result<Vec<Node>> {
            Ok(self.nodes.clone())
        }
        fn import_graph(&self) -> Result<ImportGraph> {
            Ok(self.graph.clone())
        }
        fn live(&self) -> bool {
            self.live
        }
        fn live_graph(&self) -> bool {
            self.live_graph
        }
    }

    /// A clean gate whose `file_summaries` returns a FIXED set — the freshness payload under test.
    struct SummaryEngine(Vec<FileSummary>);

    impl GateEngine for SummaryEngine {
        fn diagnostics(&mut self, _files: &[(String, String)]) -> Result<Vec<Diag>> {
            Ok(vec![])
        }
        fn rename(&mut self, _f: &str, _l: u32, _c: u32, _n: &str) -> Result<Value> {
            Ok(json!({}))
        }
        fn will_rename(&mut self, _from: &str, _to: &str) -> Result<Value> {
            Ok(json!({}))
        }
        fn file_summaries(&mut self, _files: &[String]) -> Result<Option<Vec<FileSummary>>> {
            Ok(Some(self.0.clone()))
        }
    }

    fn summary(path: &str, deleted: bool, imports: &[&str]) -> FileSummary {
        FileSummary {
            path: path.into(),
            deleted,
            nodes: vec![],
            imports: imports.iter().map(PathBuf::from).collect(),
        }
    }

    fn base_graph() -> ImportGraph {
        let mut g = ImportGraph::new();
        g.insert(PathBuf::from("a.rs"), vec![PathBuf::from("b.rs")]);
        g.insert(PathBuf::from("blank.rs"), vec![PathBuf::from("a.rs")]);
        g.insert(PathBuf::from("gone.rs"), vec![PathBuf::from("a.rs")]);
        g
    }

    fn commit_one(p: &Composed<StubReader>, root: &Path) {
        std::fs::write(root.join("a.rs"), "one\n").unwrap();
        let r = p
            .apply_edits(
                &[EditOp::ReplaceInFile { path: "a.rs".into(), old_text: "one".into(), new_text: "two".into() }],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        assert!(matches!(r, CommitResult::Ok { .. }), "clean commit expected: {r:?}");
    }

    // The post-commit graph overlay's three cases — REPLACE an entry, BLANK it (no imports
    // left), DELETE the file — must match the per-file overlay semantics the providers'
    // artifact graphs are built on (lang-rust `overlay_graph`: replace / remove-on-empty /
    // remove-on-delete). This is the equivalence the P6 migration rests on.
    #[test]
    fn post_commit_overlay_replaces_blanks_and_deletes_entries() {
        let dir = tempfile::tempdir().unwrap();
        let read = StubReader { graph: base_graph(), nodes: vec![], live: false, live_graph: false };
        let summaries = vec![
            summary("a.rs", false, &["c.rs"]), // replace: a.rs now imports c.rs
            summary("blank.rs", false, &[]),   // blank: no imports left -> entry removed
            summary("gone.rs", true, &[]),     // delete: file gone -> entry removed
        ];
        let p = Composed::new(
            dir.path(),
            read,
            Arc::new(move |_root: &Path| {
                Ok(Box::new(SummaryEngine(summaries.clone())) as Box<dyn GateEngine + Send>)
            }),
        );
        commit_one(&p, dir.path());

        let g = p.import_graph().unwrap();
        assert_eq!(
            g.get(&PathBuf::from("a.rs")),
            Some(&vec![PathBuf::from("c.rs")]),
            "changed file's entry REPLACED: {g:?}"
        );
        assert!(!g.contains_key(&PathBuf::from("blank.rs")), "import-less file's entry removed: {g:?}");
        assert!(!g.contains_key(&PathBuf::from("gone.rs")), "deleted file's entry removed: {g:?}");
    }

    // The fresh-deepener boundary: structure served FROM A FRESH SUMMARY passes through the
    // reader's read-time enrichment (lang-ts re-anchors + deepens against current disk), a
    // DELETED summary stays blank (nothing to enrich), and reads with no override never
    // invoke it — the reader's own structure() already carries the enrichment there.
    #[test]
    fn fresh_summary_structure_passes_through_the_deepener() {
        let dir = tempfile::tempdir().unwrap();
        let read = StubReader { graph: base_graph(), nodes: vec![], live: false, live_graph: false };
        let fresh_node = fn_node("a.rs", "committed_truth", 1, 3);
        let summaries = vec![FileSummary {
            path: "a.rs".into(),
            deleted: false,
            nodes: vec![fresh_node],
            imports: vec![PathBuf::from("b.rs")],
        }];
        let p = Composed::new(
            dir.path(),
            read,
            Arc::new(move |_root: &Path| {
                Ok(Box::new(SummaryEngine(summaries.clone())) as Box<dyn GateEngine + Send>)
            }),
        )
        .with_fresh_deepener(Arc::new(|rel, mut nodes: Vec<Node>| {
            for n in &mut nodes {
                n.children.push(fn_node(rel, "deepened_marker", 1, 1));
            }
            nodes
        }));

        assert!(
            p.structure(Path::new("a.rs")).unwrap().is_empty(),
            "no override yet: the artifact reader serves (and the deepener stays out of it)"
        );
        commit_one(&p, dir.path());

        let nodes = p.structure(Path::new("a.rs")).unwrap();
        let n = nodes.iter().find(|n| n.id == "a.rs#committed_truth").expect("fresh node served");
        assert!(
            n.children.iter().any(|c| c.id == "a.rs#deepened_marker"),
            "fresh-served structure must pass through the deepener: {:?}",
            n.children
        );

        // A deleted override blanks the file — the deepener must not resurrect it.
        if let Ok(mut m) = p.fresh.lock() {
            m.insert(
                "a.rs".into(),
                FileSummary { path: "a.rs".into(), deleted: true, nodes: vec![], imports: vec![] },
            );
        }
        assert!(p.structure(Path::new("a.rs")).unwrap().is_empty(), "deleted override stays blank");
    }

    // The hybrid-reader boundary (live structure × artifact graph): the freshness push runs
    // and corrects the GRAPH, but structure() keeps coming from the live reader — a fresh
    // summary must never shadow a later disk read.
    #[test]
    fn hybrid_reader_gets_graph_overlay_but_structure_stays_live() {
        let dir = tempfile::tempdir().unwrap();
        let live_nodes = vec![fn_node("a.rs", "live_truth", 1, 3)];
        let read =
            StubReader { graph: base_graph(), nodes: live_nodes.clone(), live: true, live_graph: false };
        let summaries = vec![summary("a.rs", false, &["c.rs"])];
        let p = Composed::new(
            dir.path(),
            read,
            Arc::new(move |_root: &Path| {
                Ok(Box::new(SummaryEngine(summaries.clone())) as Box<dyn GateEngine + Send>)
            }),
        );
        commit_one(&p, dir.path());

        let g = p.import_graph().unwrap();
        assert_eq!(
            g.get(&PathBuf::from("a.rs")),
            Some(&vec![PathBuf::from("c.rs")]),
            "artifact graph corrected post-commit: {g:?}"
        );
        let nodes = p.structure(Path::new("a.rs")).unwrap();
        assert_eq!(
            nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            live_nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            "structure served LIVE, not from the fresh summary"
        );
    }
}
