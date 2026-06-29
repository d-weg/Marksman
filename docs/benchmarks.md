# codeindex-rs — benchmarks

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
headless, sonnet 4.6) on the same tasks**, with and without codeindex, fully accounted. Harness:
`scripts/agent-bench/` (see its README for the trust properties — one variable, an objective
per-task `check`, clean git + index reset each run, tokens straight from Claude Code's JSON,
every task reported). Three arms: **baseline** (no tool), **rust** (codeindex-rs MCP), **ts** (the
Node `codeindex` MCP). Target repo: the Node `codeindex` itself (~600-symbol TS). **Median of 3
runs.** `sec` = wall-clock for the whole agent run.

| task | arm | in_tok | out_tok | turns | sec | ok |
|---|---|--:|--:|--:|--:|:--:|
| T1-rename | baseline | 165658 | 940 | 8 | 23 | 3/3 |
|  | **rust** | **102264** | **563** | **4** | **15** | 3/3 |
|  | ts | 107015 | 604 | 4 | 19 | 3/3 |
| T2-move | baseline | 231593 | 1190 | 11 | 31 | 3/3 |
|  | **rust** | **75195** | **386** | **3** | **12** | 3/3 |
|  | ts | 76504 | 389 | 3 | 12 | 3/3 |
| T3-locate-edit | baseline | 100928 | 384 | 4 | 13 | 3/3 |
|  | rust | 127121 | 549 | 5 | 14 | 3/3 |
|  | ts | 130575 | 559 | 5 | 18 | 3/3 |

### Totals (median per task, summed)

| arm | input tok | output tok | sec | vs baseline (in / out / time) | success |
|---|--:|--:|--:|---|--:|
| baseline | 498179 | 2514 | 67 | — | 9/9 |
| **rust** | **304580** | **1498** | **41** | **−39% / −40% / −38%** | 9/9 |
| ts | 314094 | 1552 | 49 | −37% / −38% / −27% | 9/9 |

**Headlines:**
- **An agent with codeindex does ~39% less work (tokens) and finishes ~38% faster**, all 9/9.
- **Rust ≈ the mature TS tool on tokens, but clearly faster on wall-clock** (−38% vs −27% time).
  At equal turn counts (T1 4/4, T3 5/5) rust is faster end-to-end (T1 15s vs 19s, T3 14s vs 18s)
  — native core + warm ts-morph + native embeddings. The time column is what surfaces the
  rewrite's payoff that tokens alone hide.
- **The win is concentrated in structural edits** — T2-move 3 turns vs baseline's 11, T1-rename
  4 vs 8.

**Honest caveats:**
- **T3-locate is deliberately neutral** (a trivial one-liner): baseline is marginally leaner
  (4 turns vs 5) because the tool call adds a step when the find is easy. Keeping a task the tool
  does *not* win is what makes the average credible.
- **All 9/9 succeeded, so the gate's robustness value is NOT in these numbers** — a type-checked
  edit is insurance against broken edits, and insurance doesn't pay out on the happy path. The
  measured win is efficiency; the resilience (catching an edit that breaks a caller in another
  file) would surface only on harder, error-prone tasks.
- **Scope:** 3 tasks, one single-package TS repo, sonnet 4.6, median of 3. The *shape* is robust;
  the absolute deltas are this-repo/these-tasks.
- The `ts` arm runs the original codeindex's **current** ranker; part of rust's edge may be its
  improved retrieval ranking, not only Rust speed. A clean read-vs-write isolation needs ranker
  parity.

Reproduce: `bash scripts/agent-bench/go.sh --runs 3` (needs `$ANTHROPIC_API_KEY`).
