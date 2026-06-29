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
}
