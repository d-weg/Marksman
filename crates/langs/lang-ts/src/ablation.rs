//! The `CI_TS_MODE=treesitter-gated` ablation provider: syntactic reads, the real gate.
use ci_core::{CommitResult, EditOp, EditOpts, Granularity, ImportGraph, LanguageProvider, Node, Result};
use ci_edit::Composed;
use lang_fallback::{FallbackProvider, FbLang};
use std::path::Path;
use std::sync::Arc;

use crate::engine::start_engine;

/// TypeScript with a tree-sitter READ path (the generic fallback's TS collector + its
/// relative-import graph — no scip, no index build, no Node at startup) and the SAME warm
/// gate as the full provider — i.e. [`Composed`] over the tree-sitter
/// [`ReadIndex`](ci_core::ReadIndex), the lang-template Step-1 shape. This type holds NO
/// radius or freshness wiring of its own: the reader is `live` (reads re-parse current
/// disk, no post-commit refresh needed) with syntactic edges, so the glue serves the blast
/// radius TRANSITIVELY — syntactic edges do not flatten barrels, and a one-hop radius lets
/// a barrel hide its consumers from the gate (measured: bench T9-barrel). Exists to measure
/// end to end what SCIP's compiler-accurate symbols and reference graph actually buy (see
/// docs/benchmarks.md); the registry builders construct it only under
/// `CI_TS_MODE=treesitter-gated`. Note the engine's `rename` is still project-wide (the
/// compiler finds references) — the ablated piece is the read/blast-radius fidelity, not
/// the rename.
#[derive(Clone)]
pub struct TsTreeGated {
    inner: Arc<Composed<FallbackProvider>>,
}

impl TsTreeGated {
    pub fn new(root: &Path) -> Self {
        let read = FallbackProvider::new(root, FbLang::Ts);
        let sandbox = ci_core::resolve_sandbox(root, "peashooter-ts");
        Self { inner: Arc::new(Composed::new(root, read, Arc::new(move |root: &Path| start_engine(root, &sandbox)))) }
    }
}

impl LanguageProvider for TsTreeGated {
    fn granularity(&self) -> Granularity {
        self.inner.granularity()
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        self.inner.structure(file)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        self.inner.import_graph()
    }

    fn prewarm(&self) {
        self.inner.prewarm()
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        self.inner.apply_edits(ops, opts)
    }
}
