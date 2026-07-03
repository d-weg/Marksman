use crate::error::Result;
use crate::types::{CommitResult, EditOp, EditOpts, Granularity, ImportGraph, Node};
use std::path::Path;

/// The single seam between the language-blind core and one language's tooling.
/// Each supported language is its own crate implementing this trait (modular); the
/// core never sees language specifics.
///
/// v1 has one impl: TypeScript, reading from a **SCIP** index and writing through
/// an **LSP + VFS** transaction. The trait is shaped so a language can deepen its
/// [`structure`](LanguageProvider::structure) tree from SCIP (symbol-level) to a
/// full AST (syntax-level) later — unlocking sub-symbol edits — without any core
/// change. [`granularity`](LanguageProvider::granularity) advertises which it is.
///
/// Synchronous: a call is request/response to a warm process (SCIP query, LSP
/// notification). Async lives only at the MCP edge via `spawn_blocking`.
pub trait LanguageProvider: Send + Sync {
    /// The structural depth this provider exposes (Symbol for SCIP, Ast for a full parse).
    fn granularity(&self) -> Granularity;

    /// Structure tree for one file (hot path — called per file during indexing).
    /// SCIP providers return a shallow tree (named declarations, class→method
    /// nesting); an AST provider returns a deep tree. Structure only — the core
    /// reads file text itself from each node's `range`.
    fn structure(&self, file: &Path) -> Result<Vec<Node>>;

    /// File-level import/reference graph across the repo, by the provider's own
    /// mechanism (SCIP cross-document references for TS — semantic, precise).
    fn import_graph(&self) -> Result<ImportGraph>;

    /// Apply a batch of edit ops **atomically** behind the type-check gate.
    ///
    /// The only write entry point. It stages the batch in a VFS, gates it via the
    /// language server's in-memory diagnostics, then commits to disk or rolls back —
    /// the whole batch must roll back together, so independent per-op calls won't do.
    /// A SCIP (`Symbol`-granularity) provider rejects sub-symbol ops it can't target.
    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult>;

    /// Warm any background write engine (LSP / SCIP indexer) so the first
    /// [`apply_edits`](LanguageProvider::apply_edits) is fast instead of paying a cold
    /// project load inline. Default: a no-op (providers with nothing to warm — the
    /// tree-sitter fallback, or a sidecar that warms itself inside its own process).
    fn prewarm(&self) {}

    /// Whether this provider type-checks its edits over the blast radius (nothing commits
    /// if it introduces a new type error). Default: `true` — the gated TS/Rust path. The
    /// tree-sitter fallback overrides this to `false`: its edits are structural, not verified.
    fn gated(&self) -> bool {
        true
    }
}

/// The READ half of a provider: the index the agent plans against. An artifact you load and
/// query (a SCIP index) or a live parser over current disk (tree-sitter) — never a running
/// checker; the write half is [`ci-edit`]'s `GateEngine`. The two advertised properties drive
/// the glue a composed provider wires between the halves:
///
/// - [`live`](ReadIndex::live): live readers reflect every commit by construction; artifact
///   readers need the post-commit freshness channel (engine `file_summaries` -> read
///   overrides) or their reads serve pre-edit state until the next reindex.
/// - [`semantic_edges`](ReadIndex::semantic_edges): compiler-accurate graphs flatten barrels
///   and re-exports, so a ONE-hop reverse-import radius is sound. Syntactic graphs do not —
///   the radius must be expanded transitively or a barrel hides its consumers from the gate
///   (measured: bench T9-barrel).
pub trait ReadIndex: Send + Sync {
    /// The structural depth this reader exposes (Symbol for SCIP, Ast for a full parse).
    fn granularity(&self) -> Granularity;
    /// Structure tree for one file (same contract as [`LanguageProvider::structure`]).
    fn structure(&self, file: &Path) -> Result<Vec<Node>>;
    /// File-level import/reference graph (same contract as [`LanguageProvider::import_graph`]).
    fn import_graph(&self) -> Result<ImportGraph>;
    /// True when reads come from current disk rather than a prebuilt artifact.
    fn live(&self) -> bool {
        false
    }
    /// True when graph edges are compiler-accurate (a one-hop blast radius is sound).
    fn semantic_edges(&self) -> bool {
        false
    }
}
