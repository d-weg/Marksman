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

## Cross-cutting finding  ✅ (landed as `ci-treesitter`)
Extracted `ts_range`, `syntax_node`, `leading_comment_range`, and a `body_ranges(root, def_kinds,
body_kinds)` outline driver into a new `ci-treesitter` crate; `lang-rust`, `lang-fallback`, and
`lang-ts` now depend on it and shed their private copies (each provider shrank; ~180 dup lines →
one tested 105-line crate). `tree-sitter` centralized in `[workspace.dependencies]` so the shared
`TsNode` ABI matches across crates. Done AFTER Batch 1 (needed the locked `Node`/`Range` contracts).

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

### Batch 1 — core seams (everything depends on these)  ✅
Done before the cross-cutting ts extraction (reversed from plan): the extracted helpers must
produce these `Node`/`Range` contracts, so review + lock them first.
- [x] `ci-core/src/types.rs` (278) — contracts sound; fixed a roundtrip test that discarded its
      `matches!` bool (asserted nothing).
- [x] `ci-core/src/driver.rs` — trait + docs clean, no change.
- [x] `ci-core/src/weight.rs` (376) — role table sound; de-duplicated the layer-score computation
      and multiplier formula (were copied verbatim across `layer_multipliers` /
      `compute_package_weights`) into `score_layers` + `layer_mult`. Behavior-identical.
- [x] `ci-core/src/{config,outline,error,lib}.rs` — reviewed, clean, no change.

### Batch 2 — the edit path (correctness-critical; largest file)  ✅
- [x] `ci-edit/src/lib.rs` (788) — audited blast-radius, `diag_key` (line-omission is deliberate,
      correct), the rename/move retry loops, `replace_text` uniqueness, byte/char handling: all
      sound. Two behavior-preserving dedups: extracted `is_transient_lsp_error` (the retry
      taxonomy was copied across `rename`/`will_rename`) and `node_by_id` (the id→node→Anchor
      resolve was repeated in three `apply_structural` arms). No behavior change.
- [x] `ci-vfs/src/lib.rs` (251) — overlay/commit/rollback and `byte_offset` (incl. the EOF
      position) correct; atomic by construction. No change.
- [x] `ci-lsp/src/lib.rs` (347) — JSON-RPC framing, settle/idle-quiet, `diagnostics` line = LSP
      0-based +1 (1-based, matches `anchor`/feedback), `root()`: all correct. No change.

**Deferred (out of this batch's scope):** `ci-vfs::byte_offset` and `lang-ts::point_byte` are
near-identical (1-based line / 0-based char → byte offset). A shared `ci_core` util would dedupe
them, but both already depend on `ci-core`, and it's unrelated to the edit-path audit — noting for
a later pass rather than widening Batch 2.

### Batch 3 — retrieval + index  ✅
- [x] `ci-retrieve/src/{retrieve,rrf}.rs` — RRF, `contains_word` (multibyte advance), symbol bonus,
      exact-flag survival, graph expansion: all correct, and the two past bug sites have explicit
      regression tests. No change.
- [x] `ci-index/src/*` (6 files) — BM25 (df bookkeeping), vector rank, graph reverse-derivation,
      JSON+f32 persistence, types: faithful ports, well-tested. No change.
- [x] `ci-embed/src/*` — Model2Vec embedder is a bit-exact, parity-tested port (worst cosine
      > 0.99999); unchecked indexing is all against trusted on-disk tensors. No change.
- [x] `ci-scip/src/lib.rs` — SCIP symbol-grammar parse → `Node` tree + reference-based import
      graph: correct, well-tested. No change.
- [x] `ci-build/src/lib.rs` — walk→structure→embed→persist + incremental refresh audited (row
      alignment, incremental keys sound). Extracted `forward_adjacency` (the provider-import-graph
      → string-adjacency block was verbatim in `build_index` and `update_index`). No behavior change.

**Deferred note (latent, not a live bug):** `ci-index::cosine_normalized` indexes the matrix by
`query.len()`, so a query whose length exceeds `dims` (e.g. an index built with a different model)
would panic rather than error. dims always match in practice; harden with a guard in a later pass
rather than churn the ranking hot path without a repro.

### Batch 4 — language providers  ✅
- [x] `langs/lang-ts/*` — SCIP+tree-sitter merge, `decl_with_fields` climb guard + field-range
      widening (sound, with the `sym_start` guard against climbing into the enclosing class), warm
      engine lifecycle + sidecar (deadlines, stderr capture, Drop kill/wait) all correct. Fixed a
      stray double blank line (extraction leftover); deduped intra-crate `npm_cache` into one
      `pub(crate)` fn shared by `lib.rs`/`tsmorph.rs`.
- [x] `langs/lang-rust/*` — item/`:doc` collection, `mod` resolution, the opt-in SCIP graph with
      instant `mod`-graph fallback, warm/retry: all sound. No change beyond the cross-cutting dedup.
- [x] `langs/lang-fallback/*` — Python structure/imports/outline, `NoGate` within-file textual
      rename (honest `gated:false`), no-op gate semantics: correct. No change beyond the dedup.

**Cross-cutting dedup:** the reverse-import-map construction was byte-identical in all three
providers' `apply_edits`. Extracted `ci_core::reverse_import_map(&ImportGraph)` (language-blind,
operates on the core type) with a unit test; each provider now calls it. ~21 dup lines removed.

### Batch 5 — surface + protocol  ✅
- [x] `ci-mcp/src/main.rs` — tool schemas, `resolve_symbol`/`resolve_query` addressing (conservative,
      well-documented), gated/ungated apply_edits messaging, JSON-RPC loop: all sound. No change
      beyond the shared `kind_str` dedup below.
- [x] `ci-proto/src/lib.rs` — wire conversions are complete + symmetric; framing uses `read_exact`
      (handles partial reads); `Drop` kills+waits the child. Oversized-length is moot for a trusted
      spawned sidecar. No change.
- [x] `ci-cli/src/main.rs` — index/retrieve commands clean. `ci-arch/src/lib.rs` — language-blind
      `file_suffix`, module-template detection, well-tested. `ci-walk/*` (lang/discover/workspace) —
      clean, well-tested. No change beyond the dedup.

**Cross-cutting dedup:** the `kind_str(SymbolKind) -> &'static str` mapping was duplicated verbatim
in `ci-cli` and `ci-mcp`. Added `SymbolKind::as_str()` on the core type (mirrors the existing
`PackageRole::as_str`), with a test asserting it matches the serde name; both surfaces now use it.

**Deferred (cross-binary, would need a shared home):** `choose_lang` and `model_dir` are duplicated
between `ci-cli` and `ci-mcp`. Sharing them needs a small app-support lib (they pull `FbLang` from
lang-fallback), which is a structural change beyond this quality pass — noted for later.

## Done ✅ — every box checked, `cargo test --workspace` green, `cargo clippy --workspace
## --all-targets` clean.

### Notes / deferred (no behavior change was made for these — candidates for a later pass)
- **`ci-index::cosine_normalized`** indexes the matrix by `query.len()`, so a query longer than
  `dims` (e.g. an index built with a different model) panics instead of erroring. dims match in
  practice; add a guard rather than churn the ranking hot path without a repro.
- **`byte_offset` (ci-vfs) vs `point_byte` (lang-ts)** are near-identical (1-based line / 0-based
  char → byte). A shared `ci-core` util would dedupe them; both already depend on ci-core.
- **`choose_lang` / `model_dir`** are duplicated between the `ci-cli` and `ci-mcp` binaries; sharing
  needs a small app-support lib (they reference `FbLang`).
- **What was NOT touched:** all numeric/parity-sensitive paths (RRF, BM25, the Model2Vec embedder,
  cosine) were reviewed and left byte-for-byte — they're faithful, parity-tested ports.

### Summary of changes (one commit per batch)
- B0 — cleared 16 clippy warnings (no behavior change).
- B1 — `ci-core`: deduped layer weighting (`score_layers`/`layer_mult`); fixed a no-op roundtrip test.
- Cross-cut — new `ci-treesitter` crate: `ts_range`/`syntax_node`/`leading_comment_range`/`body_ranges`
  shared by all three tree-sitter providers; centralized the `tree-sitter` dep.
- B2 — `ci-edit`: `is_transient_lsp_error` + `node_by_id` dedups (edit path verified correct).
- B3 — `ci-build`: `forward_adjacency` dedup (retrieval/index/embed/scip verified correct).
- B4 — `ci_core::reverse_import_map` shared by all providers; consolidated lang-ts `npm_cache`.
- B5 — `SymbolKind::as_str()` on the core type, shared by both binaries.
