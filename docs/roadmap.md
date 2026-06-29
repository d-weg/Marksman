# codeindex-rs — roadmap

Notes and intended directions, not commitments.

## Benchmarks (planned)

### Three-way: read/edit backend comparison — **speed AND precision**
Compare the same operations across three implementations on the same real repos:

| Variant | Read | Edit granularity | Notes |
|---|---|---|---|
| **Rust + SCIP only** | SCIP index (`scip-typescript`) | symbol-level (`replace_node`) | compiler-grade symbols/refs, no sub-symbol edits |
| **Rust + SCIP + tree-sitter** (current) | SCIP + in-process tree-sitter | **sub-symbol** (`#sym:body`/`:return`/`:param.N`) | `Granularity::Ast`; tree-sitter is in-process, no external dep |
| **Node** (original) | ts-morph | sub-symbol | the oracle |

The AST-granularity provider is **done via tree-sitter** (in-process, no Node), not ts-morph —
so the interesting comparison is now SCIP-only vs SCIP+tree-sitter vs Node.

Measure:
- **Indexing speed**: cold + warm, per repo size (small / monorepo).
- **Retrieval precision**: manifest overlap + ranking vs a labeled set; the Rust+SCIP vs
  Node 13/18 (72%) overlap on the RRF task is the current baseline.
- **Edit precision / coverage**: which edit ops each backend can serve (SCIP can't do
  sub-symbol), and output-token cost per edit class (rename, replace_node, micro-edit).
- **End-to-end edit latency**: gate round-trip (SCIP+LSP vs ts-morph in-process).

Goal: quantify the SCIP-vs-AST tradeoff we reasoned about — SCIP wins on precision-of-refs +
modularity + speed; AST wins on sub-symbol token savings. The `Granularity` seam already lets
both plug into the same core, so this is an apples-to-apples A/B.

## Other directions
- **Fine verbs over the AST tree** — `set_body`/`set_return_type`/`add_parameter`/
  `set_async` now have targets (`#sym:body`/`:return`/`:param.N`) thanks to the SCIP+tree-sitter
  merge; remaining work is mapping those verbs in `action_to_op` + non-ASCII column handling
  (tree-sitter columns are byte-based).
- **Incremental SCIP refresh** after a commit (reindex changed files; `scip-typescript` is largely
  whole-project — measure latency, consider a faster path).
- **More languages via the generic LSP gate** (pyright, gopls, rust-analyzer) — each a new crate
  implementing `LanguageProvider`.
- **Persisted package roles** (deps-based `infer_role` at index time) for sharper query weighting.
