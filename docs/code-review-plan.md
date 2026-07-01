# Marksman — file-by-file code-quality pass

A structured, low-risk sweep to raise overall code quality now that the feature surface is stable
(3 languages, gated edits, intent addressing, sidecar). **Goal:** correctness, less duplication,
clearer code — *no behavior changes* unless a fix is clearly warranted (and then with a test).
**Non-goal:** new features (those stay on [roadmap.md](roadmap.md)).

## How to run a batch
1. Read every file in the batch against the **dimensions** below; jot findings.
2. **Fix-inline** anything small + safe (clippy, dead code, stale comments, an obvious edge case).
   **Flag** anything larger (a refactor, a behavior question) as a checklist item here.
3. `cargo test --workspace` green + `cargo clippy --workspace` no new warnings after each batch.
4. One commit per batch (`refactor(<crate>): …` / `fix(<crate>): …`); optionally `/code-review`
   the batch diff before committing.

## Dimensions (the per-file checklist)
- **Correctness / edge cases** — `unwrap`/`expect`/`panic` on attacker/agent input; byte-vs-char
  boundaries (we slice UTF-8 a lot); off-by-one in ranges; empty/missing-file paths.
- **Error handling** — return `Result` over panicking; `map_err` messages that name the cause;
  no silent `unwrap_or_default` that hides a real failure.
- **Duplication** — see the cross-cutting item below; also repeated parsing/IO idioms.
- **Clarity** — function length, naming, dead code, stale comments/docs vs. current behavior.
- **Performance** — allocations + clones in hot paths (`structure()` runs per file at index time;
  retrieval fusion loops); needless re-parsing.
- **API consistency** — trait method shapes, error types, the `Node`/`EditOp`/`Range` contracts.
- **Tests** — a unit test for each non-trivial branch; a named regression for each past bug.

## Cross-cutting finding (do FIRST — it shrinks later batches)
**Tree-sitter helper duplication.** `ts_range`, the `syntax(...)` sub-node builder, the
`Node`-from-named-item construction, the leading-comment/`:doc` finder, and the body-eliding
`outline` walk are **re-implemented in `lang-rust`, `lang-fallback`, and `lang-ts/ast.rs`.** Extract
a small `ci-treesitter` (or `ci-core::ts`) crate: `ts_range(&TsNode) -> Range`, `syntax_node(id,
kind, &TsNode)`, `leading_doc_range(&TsNode, kinds)`, and an `outline(content, body_kinds,
placeholder)` driver. Each provider shrinks to *its grammar's node-kind names* — which is exactly
the per-language "wiring, not core work" the architecture promises. Land this first; batches 4
get much smaller.

## Batches (priority order — central + correctness-critical first)

### Batch 0 — clippy baseline (mechanical, ~1 sitting)
- [x] Clear the **16 existing `cargo clippy --workspace` warnings** (sort_by_key, map_or, manual
      find/iterator, loop-index, doc-indentation, etc.). Pure cleanup, no behavior change.

### Batch 1 — core seams (everything depends on these)
- [ ] `ci-core/src/types.rs` (278) — `Node`/`EditOp`/`CommitResult`/`Range`/`SymbolKind` contracts.
- [ ] `ci-core/src/driver.rs` — the `LanguageProvider` trait + docs.
- [ ] `ci-core/src/weight.rs` (376) — path-role + layer weighting; check the role inference table.
- [ ] `ci-core/src/{config,outline,error,lib}.rs` — config defaults, `elide_bodies*`, error enum.

### Batch 2 — the edit path (correctness-critical; largest file)
- [ ] `ci-edit/src/lib.rs` (788) — `action_to_op` / `commit_edits` / gate diff / `apply_*`. Audit
      the blast-radius set, `diag_key` (line intentionally omitted), the rename/move retry loops,
      `replace_text` uniqueness, byte/char handling. Split into modules if it eases review.
- [ ] `ci-vfs/src/lib.rs` (251) — overlay + commit/rollback; range→byte mapping; atomicity.
- [ ] `ci-lsp/src/lib.rs` (347) — JSON-RPC framing, the settle/idle-quiet logic, `root()`.

### Batch 3 — retrieval + index
- [ ] `ci-retrieve/src/retrieve.rs` (553) — RRF, `contains_word`, symbol bonus, graph expansion;
      the char-boundary advance + exact-flag preservation (past bug sites).
- [ ] `ci-index/src/*` (514, 6 files) — BM25, vector store, graph store, persistence/snapshot.
- [ ] `ci-embed/src/*` (218) — Model2Vec tokenize + safetensors; parity invariants.
- [ ] `ci-scip/src/lib.rs` (244) — SCIP reader → `Node` tree + import graph.
- [ ] `ci-build/src/lib.rs` (452) — the walk→structure→embed→persist pipeline; incremental keys.

### Batch 4 — language providers (shrinks a lot after the cross-cutting refactor)
- [ ] `langs/lang-ts/src/{ast,lib,tsmorph,sidecar,outline}.rs` (880) — SCIP+tree-sitter merge,
      `decl_with_fields` climb guard, field-range widening, the warm-engine lifecycle, sidecar.
- [ ] `langs/lang-rust/src/{lib,sidecar}.rs` (601) — item/`:doc` collection, `mod` resolution,
      rust-analyzer warm/retry, sidecar.
- [ ] `langs/lang-fallback/src/lib.rs` (683) — Python structure/imports/outline, the `NoGate`
      within-file rename, `gated:false` semantics.

### Batch 5 — surface + protocol
- [ ] `ci-mcp/src/main.rs` (679) — tool schemas, resolution (`resolve_symbol`/`resolve_query`),
      apply_edits messaging; consider splitting tool handlers into a module.
- [ ] `ci-proto/src/lib.rs` (517) — wire conversions, framing robustness (partial reads, oversized
      length), the `Drop` kill/wait.
- [ ] `ci-cli/src/main.rs` (231), `ci-arch/src/lib.rs` (240), `ci-walk/src/*` (243).

## Done = every box checked, suite green, clippy clean, and a short notes section here recording
any behavior questions deferred to the roadmap.
