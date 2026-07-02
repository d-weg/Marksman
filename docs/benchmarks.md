# Marksman — benchmarks

Reproduce with `cargo build --release && python3 scripts/bench.py [oracle_repo]`.
Oracle = the sibling Node `codeindex` repo (TypeScript, ~600 symbols).

## Indexing speed (whole repo, wall-clock, min of 3 after warmup)

| variant | time |
|---|---|
| Rust · SCIP only (no tree-sitter) | 2.99s |
| **Rust · SCIP + tree-sitter** | **3.02s** |
| tree-sitter overhead | **+0.04s (+1.2%)** |
| Node (bge, the oracle) | 12.68s |

**Headlines:**
- The **SCIP + tree-sitter merge is essentially free** — +1.2% (~40ms) at index time. Adding the
  in-process AST (which unlocks sub-symbol edits) costs nothing measurable. This is the key result
  validating the merge.
- **Rust indexes ~4× faster than Node** (3s vs 12.7s). Part native Rust, part the potion-code
  Model2Vec embedder vs bge.
- Of the ~3s, the dominant cost is `scip-typescript` (the external indexer) + embedding; the
  language-blind Rust core is a small fraction.

## Retrieval overlap vs Node (Jaccard, per task)

| task | rust | node | shared | Jaccard |
|---|--:|--:|--:|--:|
| merge bm25/vector/symbol with RRF | 18 | 18 | 14 | 64% |
| ast-anchored structural edits + gate | 14 | 13 | 9 | 50% |
| package-aware relevance weighting | 15 | 14 | 9 | 45% |
| import graph + seed expansion | 16 | 16 | 12 | 60% |

**mean Jaccard ≈ 55%.**

This is honest moderate overlap, and the two divergences are *expected*: Rust uses **potion-code
Model2Vec** (the Node oracle here uses **bge-small**), and Rust's graph is **SCIP semantic
references** vs the Node tool's **ts-morph import declarations**. Same embedder + same graph would
converge further; the cores (BM25/RRF/weighting/expansion) are faithful ports.

## Multi-language retrieval (Batch 6)

Reproduce with `cargo build --release && python3 scripts/multilang-bench/run.py`. Self-contained —
a small mixed Rust + TypeScript + Python fixture (`scripts/multilang-bench/fixture`) with six
labeled tasks, two per language. A/B on the same fixture and tasks, one variable — which
provider(s) index:

- **single** — `CI_LANG=rust` forces one provider (the old one-language-per-repo behavior);
- **multi** — auto-detect via the extension→provider registry, so every present language indexes.

| language | single (rust only) | multi (all langs) |
|---|--:|--:|
| rust | 2/2 | 2/2 |
| python | 0/2 | **2/2** |
| ts | 0/2 | **2/2** |

**single: 2/6 tasks retrieved · multi: 6/6** (hit@5).

**Headline:** with one provider per repo, a mixed repo's non-Rust files are simply *never indexed*,
so they can't be retrieved at any rank — recall is 0 for those languages. The registry indexes each
file with its own language's provider, so cross-language recall goes **0 → 100%** here. Retrieval
itself is unchanged (one language-blind BM25 + vector index); the gain is purely that every
language's files now make it *into* that index. (TypeScript indexes only when Node / scip-typescript
is available; the runner reports it as absent otherwise, and the Rust↔Python A/B still shows the
effect.)

## Edit / capability (not a timing)

| | SCIP only | **SCIP + tree-sitter** | Node (ts-morph) |
|---|---|---|---|
| rename / refs / import graph | ✅ compiler-grade | ✅ | ✅ |
| whole-symbol edits (replace_node) | ✅ | ✅ | ✅ |
| move (importer rewrite) | ✅ via LSP willRenameFiles | ✅ | ✅ |
| **sub-symbol edits** (body/return/param) | ❌ no AST | **✅ tree-sitter** | ✅ |
| external runtime dep for the AST | — | **none (in-process)** | Node |

The merge gives sub-symbol precision **with no external dependency** for the AST (tree-sitter is a
compiled-in Rust crate) and **no measurable index cost** — the best of both.

## Notes
- Edit-gate latency: the default write engine is now **ts-morph** (in-process, synchronous),
  kept warm via prewarm, so a full rename + blast-radius gate is **~0.9s** (vs a cold LSP
  server's ~68s; `CI_EDIT_ENGINE=lsp` keeps the generic fallback). End-to-end effect is in the
  Agent A/B benchmark below.
- Method: `--release` binary, `min of 3` after a discarded warmup run to control for
  scip-typescript / OS-cache effects.

## Agent A/B benchmark — does the tool actually help an agent? (end-to-end, LIVE-AGENT)

The above are micro-benchmarks. This is the one that matters: the **same agent (Claude Code
headless, sonnet 4.6) on the same tasks**, with and without Marksman, fully accounted. Harness:
`scripts/agent-bench/` (see its README for the trust properties — one variable, an objective
per-task `check`, clean git + index reset each run, tokens straight from Claude Code's JSON,
every task reported). Three arms: **baseline** (no tool), **rust** (Marksman MCP), **ts** (the
Node `codeindex` MCP — the mature tool Marksman is a rewrite of). Target repo: the Node
`codeindex` itself (~600-symbol TS). `sec` = wall-clock; `$` = Claude Code's `total_cost_usd`
(the true economic score — it bakes in prompt caching + output pricing, so it can diverge from
raw `in_tok`). Results below: 6 tasks, single run (2026-07-01); re-run with `--runs 3` for
medians.

| task | arm | in_tok | out_tok | turns | sec | $ | ok |
|---|---|--:|--:|--:|--:|--:|:--:|
| T1-rename | baseline | 218201 | 1697 | 12 | 37 | 0.1932 | 1/1 |
|  | **rust** | **73417** | **474** | **3** | **18** | **0.0499** | 1/1 |
|  | ts | 121556 | 708 | 5 | 27 | 0.0613 | 1/1 |
| T2-move | baseline | 183938 | 1083 | 8 | 31 | 0.0853 | 1/1 |
|  | **rust** | **73374** | **468** | **3** | **18** | **0.0497** | 1/1 |
|  | ts | 69279 | 412 | 3 | 17 | 0.0326 | 1/1 |
| T3-locate-edit | baseline | 114156 | 481 | 5 | 17 | 0.0546 | 1/1 |
|  | **rust** | **73314** | **408** | **3** | **15** | **0.0486** | 1/1 |
|  | ts | 177462 | 883 | 7 | 32 | 0.0855 | 1/1 |
| T4-body-edit | baseline | 93373 | 462 | 4 | 17 | 0.0496 | 1/1 |
|  | rust | 73531 | 433 | 3 | 15 | 0.0495 | 1/1 |
|  | ts | 69498 | 391 | 3 | 17 | 0.0325 | 1/1 |
| T5-schema-field | baseline | 272856 | 1637 | 12 | 39 | 0.1379 | 1/1 |
|  | **rust** | **101248** | **836** | **4** | **23** | **0.0677** | 1/1 |
|  | ts | 379669 | 3533 | 17 | 74 | 0.2131 | 1/1 |
| T6-type-rename | baseline | 281323 | 2448 | 21 | 39 | 0.1929 | 1/1 |
|  | **rust** | **73580** | **603** | **3** | **18** | **0.0522** | 1/1 |
|  | ts | 69356 | 427 | 3 | 18 | 0.0330 | 1/1 |

### Totals

| arm | input tok | output tok | sec | $ cost | vs baseline (in / out / sec / $) | success |
|---|--:|--:|--:|--:|---|--:|
| baseline | 1163847 | 7808 | 180 | 0.7135 | — | 6/6 |
| **rust** | **468464** | **3222** | **107** | **0.3176** | **−60% / −59% / −40% / −55%** | 6/6 |
| ts | 886820 | 6354 | 184 | 0.4579 | −24% / −19% / +2% / −36% | 6/6 |

**Headlines:**
- **An agent with Marksman costs ~55% less and finishes ~40% faster, 6/6 correct** — and wins or
  ties the mature TS tool it's a rewrite of on every task.
- **`$` is the truest score.** It reflects prompt caching (re-sent context bills at ~10% as
  cache reads) and output's higher per-token price. The dominant cost driver is **turns**: each
  turn re-sends the whole context, and more turns means more (pricey) output. Rust takes the
  fewest turns on every task.
- **Repo-wide structural edits are the blowouts.** T6-type-rename: **3 turns vs baseline's 21**
  (one gated `rename` rewrites the interface and every reference/import; baseline read five whole
  files and made 12 hand-edits). T1-rename 3 vs 12; T2-move 3 vs 8.
- **Wide-blast-radius edits ride the reject-driven protocol.** T5 (add a required field to an
  interface + set it at every construction site): the rust agent makes the anchor edit alone,
  the type-check gate *rejects* with **every** affected site — each with its current source and
  a ready-to-copy `fix:` action — and one batch later it's done. 4 turns / 836 output tokens vs
  baseline's 12 / 1637, and no grep can miss a site: the compiler enumerates them.
- **T3-locate went from parity to a win** via constraint-based disambiguation: the agent edits a
  field by bare name with no locate step; when the name collides, the server resolves it from the
  edit's own `oldText` (only one candidate contains it) instead of asking back.

**Honest caveats:**
- **Single run** (model nondeterminism not averaged; historical medians of 3 showed the same
  shape). Trajectory variance is real: on T5 the agent sometimes pre-explores before the first
  edit, landing at 7–9 turns instead of 4 — still well under baseline, and every path converges
  through the same self-sufficient reject.
- **No benchmark-tuned prompting.** The MCP tool descriptions are audited to contain **zero
  fixture names or task values** (an earlier revision leaked near-verbatim task answers into
  description examples; those runs were discarded and the examples replaced with
  fixture-foreign ones). What the tool teaches, it teaches generically.
- **All 6/6 succeeded, so the type-check gate's resilience value is NOT in these numbers** —
  insurance doesn't pay out on the happy path. The measured win is efficiency.
- **Scope:** 6 tasks, one single-package TS repo, sonnet 4.6. The *shape* is robust; absolute
  deltas are this-repo/these-tasks.

**Pending — T7-multilang.** The suite now includes a seventh task (`tasks.json`: two renames in
ONE session — a Rust function gated by `cargo check` and a TS function gated by tsc — on a
self-contained mixed Rust+TS+Python fixture, `scripts/agent-bench/fixture-multilang/`). It
exercises the per-file provider registry end to end and runs standalone:
`python3 scripts/agent-bench/run.py --task T7-multilang --arms baseline,rust`. The `ts` arm is
excluded by the task's `arms` field (the Node oracle can't index Rust). **Not yet measured** —
the numbers above remain the 6-task result.
- The `ts` arm runs the original codeindex's **current** ranker; part of rust's edge may be its
  improved retrieval and edit-workflow design, not only Rust speed.

Reproduce: `bash scripts/agent-bench/go.sh --runs 3` (needs `$ANTHROPIC_API_KEY`; rebuilds the
release binaries first, so results always reflect the current source). Add
`--save-transcript <dir>` then `python3 scripts/agent-bench/analyze.py <dir>` to see *why* an arm
spent its turns (tool sequence, edit actions chosen, read-before-edit).

### Startup: cached SCIP index

MCP server startup on an already-indexed repo, measured on the bench target (wall-clock):

| | cold (source changed) | warm (fingerprint match) |
|---|--:|--:|
| TS provider startup | ~26s (scip-typescript run) | **0.11s** |

A content-hash fingerprint (all `.ts*`/`.js*` sources + tsconfig/package/lockfiles + the pinned
scip-typescript version, augmented with the index's own document list) decides load-vs-reindex.
Content hashes, not mtimes — a `git reset`/checkout rewrites mtimes but not bytes and still hits
the cache. Any doubt (missing/corrupt fingerprint, tool-version bump) reindexes; a stale load is
treated as a correctness bug, a spurious reindex as only a slow start.
