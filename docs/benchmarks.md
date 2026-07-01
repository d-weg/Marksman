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
`codeindex` itself (~600-symbol TS). **Median of 3 runs.** `sec` = wall-clock; `$` = Claude
Code's `total_cost_usd` (the true economic score — it bakes in prompt caching + output pricing,
so it can diverge from raw `in_tok`).

| task | arm | in_tok | out_tok | turns | sec | $ | ok |
|---|---|--:|--:|--:|--:|--:|:--:|
| T1-rename | baseline | 162316 | 1072 | 10 | 26 | 0.0890 | 3/3 |
|  | **rust** | **100548** | **569** | **4** | **15** | **0.0486** | 3/3 |
|  | ts | 103159 | 563 | 4 | 19 | 0.0535 | 3/3 |
| T2-move | baseline | 203942 | 1317 | 11 | 32 | 0.1010 | 3/3 |
|  | **rust** | **73625** | **394** | **3** | **12** | **0.0349** | 3/3 |
|  | ts | 73097 | 357 | 3 | 13 | 0.0325 | 3/3 |
| T3-locate-edit | baseline | 122909 | 468 | 5 | 13 | 0.0515 | 3/3 |
|  | rust | 125141 | 529 | 5 | 15 | 0.0570 | 3/3 |
|  | ts | 126021 | 552 | 5 | 18 | 0.0565 | 3/3 |
| T4-body-edit | baseline | 100160 | 448 | 4 | 15 | 0.0455 | 3/3 |
|  | **rust** | **73402** | **411** | **3** | **11** | **0.0308** | 3/3 |
|  | ts | 124772 | 586 | 5 | 18 | 0.0525 | 3/3 |

### Totals (median per task, summed)

| arm | input tok | output tok | sec | $ cost | vs baseline (in / out / sec / $) | success |
|---|--:|--:|--:|--:|---|--:|
| baseline | 589327 | 3305 | 86 | 0.2869 | — | 12/12 |
| **rust** | **372716** | **1903** | **53** | **0.1713** | **−37% / −42% / −38% / −40%** | 12/12 |
| ts | 427049 | 2058 | 69 | 0.1949 | −28% / −38% / −20% / −32% | 12/12 |

**Headlines:**
- **An agent with Marksman costs ~40% less and finishes ~38% faster**, all 12/12 — and now
  **beats the mature TS tool it's a rewrite of on every axis** (rust −40% cost vs ts −32%).
- **`$` is the truest score.** It reflects prompt caching (re-sent context bills at ~10% as
  cache reads) and output's higher per-token price. The dominant cost driver is **turns**: each
  turn re-sends the whole context, and more turns means more (pricey) output. Rust takes the
  fewest turns on every task — that's the win.
- **Concentrated in structural + surgical edits.** T1-rename 4 turns vs baseline's 10 (one
  `apply_edits` rename rewrites every reference); T2-move 3 vs 11. And T4-body-edit is the clearest
  rust-vs-ts split: **rust −27% vs ts +25%** — because rust exposes `replace_text` (swap an exact
  substring, gated, no read, no body re-emit), so its agent does the edit in one call while the
  Node tool's agent reads and re-emits the whole function.

**Honest caveats:**
- **T3-locate is parity, by construction** (rust +2%, noise). It's a trivially-greppable one-line
  field edit — grep's home turf. An MCP arm pays a fixed tool-discovery turn or two that a
  one-line edit can't amortize, so the realistic floor here is *parity*, not a win. Keeping a task
  the tool does not win is what makes the average credible. (Earlier runs spiked when a real bug —
  field nodes having a name-only edit range — made `replace_text` on a field fail; that's fixed,
  and the variance with it.)
- **All 12/12 succeeded, so the type-check gate's resilience value is NOT in these numbers** — a
  gated edit is insurance against broken edits, and insurance doesn't pay out on the happy path.
  The measured win is efficiency; catching an edit that breaks a caller in another file would
  surface only on harder, error-prone tasks.
- **Scope:** 4 tasks, one single-package TS repo, sonnet 4.6, median of 3. The *shape* is robust;
  absolute deltas are this-repo/these-tasks.
- The `ts` arm runs the original codeindex's **current** ranker; part of rust's edge may be its
  improved retrieval ranking, not only Rust speed. A clean read-vs-write isolation needs ranker
  parity.

Reproduce: `bash scripts/agent-bench/go.sh --runs 3` (needs `$ANTHROPIC_API_KEY`). Add
`--save-transcript <dir>` then `python3 scripts/agent-bench/analyze.py <dir>` to see *why* an arm
spent its turns (tool sequence, edit actions chosen, read-before-edit).
