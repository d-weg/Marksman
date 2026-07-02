# Marksman — benchmarks

Two kinds of evidence, in order of importance:

1. **[The live-agent A/B](#1-headline--the-live-agent-ab)** — the same agent with and without
   Marksman, end-to-end, objectively checked. This is the number that matters.
2. **[The read-path ablation](#2-what-each-layer-buys--the-read-path-ablation)** — which *layer*
   of the tool (compiler gate, import-graph radius, SCIP edges) buys which part of that win.
   This is what decides [how new languages roll out](#3-what-this-settles--the-provider-rollout-ladder).

[Micro-benchmarks](#4-micro-benchmarks) (index speed, retrieval overlap, startup) and
[reproduction](#5-reproduce) at the end.

---

## 1. Headline — the live-agent A/B

The same agent (Claude Code headless, sonnet 4.6), the same tasks, the same repos; the ONLY
variable is whether the Marksman MCP is loaded. Harness: `scripts/agent-bench/` (see its README
for the trust properties: objective per-task `check`, clean git + index reset per run,
`--strict-mcp-config` on every arm, tokens straight from Claude Code's JSON, every task
reported, no subagents). `$` = Claude Code's `total_cost_usd` — the true economic score (it
bakes in prompt caching and output pricing); **turns** is the robust column for single runs
(cache-creation vs cache-read pricing can move `$` at identical token counts).

### Full 10-task suite (single runs, 2026-07-02)

Six tasks on a ~600-symbol single-package TS repo, plus four self-contained fixtures:
mixed Rust+TS (T7), Python+Go via the generic fallback (T8), a barrel-heavy TS repo (T9),
and a TS workspace monorepo (T10).

| arm | input tok | output tok | sec | $ | vs baseline (in/out/sec/$) | success |
|---|--:|--:|--:|--:|---|--:|
| baseline | 1,982,003 | 15,726 | 331 | 1.1989 | — | 10/10 |
| **rust (Marksman)** | **990,222** | **8,388** | **194** | **0.6648** | **−50% / −47% / −42% / −45%** | **10/10** |

| task | what it tests | baseline turns / $ | rust turns / $ |
|---|---|--:|--:|
| T1-rename | cross-file function rename | 10 / 0.1621 | **3 / 0.0477** |
| T2-move | file move + importer rewrite | 12 / 0.1037 | **3 / 0.0489** |
| T3-locate-edit | find + change one default | 5 / 0.0486 | **3 / 0.0487** |
| T4-body-edit | surgical two-spot body edit | 4 / 0.0484 | 3 / 0.0495 |
| T5-schema-field | required field + all construction sites | 13 / 0.1354 | **4 / 0.0673** |
| T6-type-rename | interface rename across 5 files | 22 / 0.2444 | **3 / 0.0496** |
| T7-multilang | Rust + TS renames, two compilers, one session | 13 / 0.1243 | **4 / 0.0522** |
| T8-fallback | Python + Go renames, generic UNGATED provider | 8 / 0.0967 | **6 / 0.0811** |
| T9-barrel | required field consumed through a barrel (`export *`) | 17 / 0.1485 | **7 / 0.1337** |
| T10-monorepo | required field consumed cross-package (`@acme/core`) | 10 / 0.0869 | **5 / 0.0860** |

### What wins where

- **Repo-wide structural edits are the blowouts.** T6: **3 turns vs 22** — one gated `rename`
  rewrites the interface and every reference/import; baseline reads five whole files and
  hand-edits each. Same shape on T1 and T2.
- **Wide-blast-radius edits ride the reject-driven protocol** (T5, T9, T10): the agent makes
  the anchor edit alone; the type-check gate *rejects* with **every** affected site — each with
  its current source and a ready-to-copy `fix:` action — and one batch later it's done. No grep
  can miss a site; the compiler enumerates them. This works **through barrels** (T9) and
  **across package boundaries** (T10).
- **Two compilers, one session** (T7): a Rust rename gated by rust-analyzer and a TS rename
  gated by tsc, each one `apply_edits`, via per-file provider dispatch.
- **Languages with no integration still win** (T8): the generic tree-sitter provider applies
  structural edits honestly marked `gated: false`, with a server-side verification scan whose
  hits carry verbatim-executable fixes ([the three-iteration story](#25-the-response-design-lesson-t8-v1v3)).

### Honest caveats

- **Single runs** (historical 3-run medians showed the same shape). Trajectory variance is
  real — T5-shaped tasks occasionally pre-explore (7–9 turns instead of 4); every path
  converges through the same self-sufficient reject.
- **No benchmark-tuned prompting.** Tool descriptions are audited to contain zero fixture
  names or task values (an early leak was caught and those runs discarded).
- **Every arm passed everything, so the gate's *resilience* value is not in these numbers** —
  insurance doesn't pay out on the happy path. The measured win is efficiency. (The ablation
  below is where unsoundness became measurable — and got fixed.)
- Absolute deltas are these-repos/these-tasks; the shape is what's robust.

### Earlier 3-arm run (vs the Node prototype)

The first 7-task run also carried a **ts** arm — the frozen Node `codeindex` prototype Marksman
rewrote. Rust: **−53% $, 7/7, winning or tying the prototype on every task** (T1 3 turns vs its
5; T5 4 vs 15). Historical detail (including the T5 contamination that led to
`--strict-mcp-config` everywhere) is in git history (`docs/benchmarks.md` @ 43d4caf); the ts
arm stays opt-in and unmaintained.

---

## 2. What each layer buys — the read-path ablation

`CI_TS_MODE` swaps the TypeScript read path so the SAME suite isolates each layer:
**`full`** (SCIP + tree-sitter + ts-morph gate, the default) · **`treesitter-gated`**
(tree-sitter reads + syntactic import graph + the same ts-morph gate; no scip, no Node at
startup) · **`treesitter`** (pure tree-sitter, ungated). Findings, as a ladder:

1. **The compiler GATE carries the agent value.**
2. **The gate is only as good as its blast RADIUS** (T9: barrels hide consumers from a naive
   syntactic radius → fixed with a transitive closure).
3. **The radius is only as good as its EDGES** (T10: bare workspace specifiers give a
   syntactic graph *no* cross-package edges — only a compiler can mint them; this is what
   SCIP is for).

### 2.1 Whole-suite: full vs treesitter-gated (10 tasks, 2026-07-02)

| arm | $ | vs baseline | success | turns per task (T1…T10) |
|---|--:|--:|--:|---|
| `full` | 0.6648 | **−45%** | 10/10 | 3 3 3 3 4 3 4 6 7 5 |
| `treesitter-gated` | 0.7639 | **−36%** | 10/10 | 3 3 3 3 4 3 4 6 6 8 |

**Gated is a dead tie on eight of ten tasks** — identical turns, $ within noise. The whole
SCIP premium (~10 points of the win) is concentrated where its semantic edges exist and the
syntactic graph's don't:

- **T9-barrel**: tie *after* the transitive-radius fix (gated 6 turns / $0.126 vs full 7 /
  $0.134) — the barrel gap was radius-depth, and it's closed.
- **T10-monorepo**: gated pays **8 turns / $0.150 vs full's 5 / $0.086 (+74%)** — exactly
  baseline's cost ($0.087). The syntactic graph has zero cross-package edges (a bare
  `@acme/core` is indistinguishable from a third-party package), so the agent hand-verifies
  what the gate can't see: the tool is paid for and its advantage evaporates.

So: **tree-sitter + gate already beats baseline decisively (−36%) with zero startup
dependencies; SCIP converts the worst cases (monorepos, cross-package blast radius) from
break-even into wins and keeps the gate's radius sound without hand-verification.**

### 2.2 The ungated tier: where it's right and where it loses

Pure tree-sitter (no gate) on the same suite: **$1.14, 8/9 — loses to baseline** on a typed
language. T5 fails outright (no compiler = no consequence enumeration, the one thing nothing
syntactic recovers), T6 "passes" at 25 turns / 2× baseline. But T4/T8 stay clean, and it even
passed T9 (11 turns) — because its reply honestly says "NOT type-verified", the agent
hand-verifies like a baseline. The tier is **right where the language has no checker**
(Python/Go/… — T8's −63% at its best) and **wrong wherever a compiler exists but isn't used**.
Corollary: a *false* "type-checked" claim is worse than an honest "unverified" — the pre-fix
T9 gated run was the cheapest, fastest arm and the only one that shipped a broken repo.

### 2.3 T9-barrel — radius depth (measured, then fixed)

`fixture-barrels`: every consumer imports through `core/index.ts` (`export *`); task =
required field + all construction sites. The gate expands reverse-import hops from the
provider's graph; a syntactic graph edges consumers to the *barrel*, which itself never errors
on a new required field — so with a naive one-hop radius the consumers sat outside the gate:

| arm (pre-fix) | turns | $ | ok |
|---|--:|--:|:--:|
| baseline | 17 | 0.1939 | 1/1 |
| `full` | 6 | 0.1142 | 1/1 |
| `treesitter-gated` | **5** | **0.0945** | **0/1 — cheapest, fastest, WRONG** (false "clean") |

**Fixed:** syntactic graphs now serve the gate the **transitive** reverse-importer set
(`ci_core::transitive_reverse_imports`); scip keeps the cheaper one-hop (its semantic graph
already flattens barrels — a consumer edges directly to the defining file). The
`barrel_consumer_…` e2e pins both mechanisms; post-fix agent runs tie full (6 turns, 1/1).

### 2.4 T10-monorepo — edge existence (SCIP-only, measured)

`fixture-monorepo`: workspace packages import core via the **bare specifier** `@acme/core`
(root-tsconfig `paths`). Unlike T9 this is not a depth problem — the syntactic resolver
follows only relative specifiers, so there are **zero cross-package edges to close over**.
The `monorepo_bare_specifier_…` e2e verifies both sides: full resolves the alias (reject
names the consumer in the other package; the ts-morph gate surfaces cross-package diagnostics
through the root tsconfig), gated commits "clean" across a broken package boundary.

| arm | turns | $ | ok |
|---|--:|--:|:--:|
| baseline | 9–10 | ~0.09–0.15 | 1/1 |
| `full` | 5–6 | 0.086–0.106 | 1/1 (−30% and better) |
| `treesitter-gated` | 8–9 | 0.150–0.152 | 1/1 *only because that trajectory re-verified by hand* — at baseline cost |

(Two runs each; project-references / multi-tsconfig monorepos remain untested.)

### 2.5 The response-design lesson (T8, v1→v3)

The generic (ungated) tier only started winning when the tool's **responses** stopped
delegating verification. Three iterations on the same Python+Go task:

| response design | rust $ | turns | outcome |
|---|--:|--:|---|
| v1: "…review or run the project's own checks" | 0.1183 | 10 | **loses +24%** — agent re-verifies by hand |
| v2: + server-side rename scan (file:line evidence) | 0.1202 | 9 | −28%, but 3 failed fix attempts (agent anchored by the now-gone OLD name) |
| v3: + every hit carries a **verbatim-executable `fix:`** (post-rename anchor) | **0.0622** | **5** | **−63%** |

The transferable law (same mechanism as the reject-driven fixes): **a response that leaves the
agent any "check it yourself" or "figure out the addressing" step is a design bug** — do the
check server-side, inline the evidence, make every follow-up copy-paste executable. Response
tokens are cheap (cached input); turns are expensive.

### Also flushed out by the ablation (fixed, regression-tested)

- **Same-file batch corruption:** structural ops resolve spans from pre-batch disk truth, so a
  schema op + a ready fix in the same file corrupted each other → `commit_edits` applies
  same-file structural ops bottom-up.
- **LSP gate blind to created files:** a didOpen overlay at a not-on-disk path is outside the
  server's project (phantom "cannot find module" on moves) → created paths are materialized
  transiently for the check, drop-guard removed on reject, invisible to the baseline pass.

---

## 3. What this settles — the provider rollout ladder

The decisions this data supports, adopted as project policy:

1. **TS and Rust keep SCIP, permanently.** `full` is the default and not removable — the
   ablation modes exist for measurement, not as product configurations. SCIP is cheap once
   cached (~0.1s warm start) and it is what keeps the gate's blast radius *sound* without
   hand-verification on barrels, re-exports, and package boundaries.
2. **New language providers land as tree-sitter + compiler gate first.** That tier is measured
   at **−36% vs baseline with zero startup dependencies** — already most of the win. The
   `TsTreeGated` provider is the template: generic tree-sitter reads + the language's real
   checker as `GateEngine` + the transitive syntactic radius. A SCIP indexer (scip-python,
   scip-go, …) is the *maturity step*, added when the language's users hit the seams where it
   is load-bearing: monorepos / cross-package imports (T10) and radius precision on large
   repos (a transitive syntactic closure over-approximates; scip's one-hop set stays bounded
   by actual referencers).
3. **The ungated tier is only for languages without a usable checker** — and its replies must
   keep saying so (`gated: false`, "NOT type-verified"): the honest weak claim measurably
   outperforms a false strong one.

---

## 4. Micro-benchmarks

Component-level numbers (reproduce: `cargo build --release && python3 scripts/bench.py
[oracle_repo]`; oracle = the sibling Node `codeindex` repo, TS, ~600 symbols).

### Indexing speed (whole repo, wall-clock, min of 3 after warmup)

| variant | time |
|---|---|
| Rust · SCIP only (no tree-sitter) | 2.99s |
| **Rust · SCIP + tree-sitter** | **3.02s** |
| tree-sitter overhead | **+0.04s (+1.2%)** |
| Node (bge, the oracle) | 12.68s |

The SCIP + tree-sitter merge is essentially free — the in-process AST (which unlocks
sub-symbol edits) costs nothing measurable at index time. Rust indexes ~4× faster than the
Node prototype; the dominant cost is `scip-typescript` + embedding, not the core.

### Startup: cached SCIP index

| | cold (source changed) | warm (fingerprint match) |
|---|--:|--:|
| TS provider startup | ~26s (scip-typescript run) | **0.11s** |

A content-hash fingerprint (all sources + tsconfig/package/lockfiles + pinned tool version,
augmented with the index's own document list) decides load-vs-reindex. Content hashes, not
mtimes — a `git reset` still hits the cache. Any doubt reindexes: a stale load is a
correctness bug, a spurious reindex only a slow start.

### Headroom on a real repo (microsoft/TypeScript, no-agent)

The fixtures are tiny by design; this measures the same machinery on the TypeScript compiler
itself — 709 source files, **453k lines, 21,794 symbols** (test corpus excluded, as any real
user would). One machine, single runs, 2026-07-02:

| what | measured |
|---|--:|
| cold full index (scip + embed + persist) | **29.5s** |
| warm reindex (`marksman index` again) | 13.9s |
| warm provider open (fingerprint check over the whole repo) | **~0.3s** |
| `retrieve` per query, end-to-end incl. process + model + index load | **0.26–0.40s** |
| gated edit, leaf file (warm engine) | **0.098s** |
| gated edit, hub file — `corePublic.ts`, 303 real importers | ~20s |

- **Retrieval's O(n) is a non-issue at this scale** (22k chunks); results were on-target
  (the union-type-narrowing query led with `compiler/types.ts`' `UnionType`).
- **Gate latency scales with the TRUE blast radius, not repo size**: ~0.1s + ~65ms per
  radius file. A hub edit checking 303 real referencers costs ~20s — the honest price of
  enumerating every consequence, still under a full `tsc` iterate, and leaf edits (the common
  case) stay ~0.1s.
- **The radius data settles the ablation's scale question brutally.** This repo imports
  through `_namespaces` barrels, so the syntactic graph sees **1** importer of
  `compiler/core.ts` where scip sees **465** true referencers — and the transitive syntactic
  closure needed for soundness is **596 files (the entire repo) for any hub edit**
  (inflation over all files: p50 1.0×, p90 59×, p99 298×). At ~65ms/file that's a ~40s gate
  per hub edit in `treesitter-gated` mode vs scip's bounded true-referencer set. On barrel-
  architected repos at scale, scip is what keeps the gate both sound AND affordable.
- **This test also flushed out a real bug** (fixed + regression-tested): the atomic index
  save swapped the whole `.marksman` dir, destroying the co-located scip cache + fingerprint
  on every save — every post-`index` startup silently paid a full scip rerun. The swap now
  carries over every file it didn't write.

Caveats: single machine/runs; `npm ci` failed in this sandbox (TypeScript's src is
self-contained so scip was unaffected — on dependency-heavy repos, install first);
`.d.ts` lib files have no syntactic import edges (scip-only, consistent with the monorepo
finding). Agent-level A/B on external repos needs repo-specific tasks with objective checks —
the harness takes `--repo`, tasks live in `scripts/agent-bench/tasks.json`.

### Composition with a context-compression proxy (Headroom, measured)

Marksman and token-compression proxies attack different waste, so we measured the
composition: the full 10-task suite through [Headroom](https://github.com/headroomlabs-ai/headroom)
0.28 (`headroom proxy` + `ANTHROPIC_BASE_URL`), single runs vs two same-day unwrapped controls.

| arm | $ (wrapped) | $ (controls) | success |
|---|--:|--:|--:|
| baseline | 1.4604 | 1.0090 / 1.1391 | 10/10 |
| rust (Marksman) | 0.5909 | 0.6470 / 0.7056 | 10/10 |

- **No protocol breakage.** All 20 wrapped tasks passed, including the reject-driven T5/T9/T10.
  The proxy's own router explains why: it *protects error outputs* and skips small content and
  tool schemas — Marksman's responses (small, dense, error-anchored, verbatim-executable) are
  exactly the content classes a well-behaved compressor leaves alone. Structurally disjoint.
- **No measurable saving on this suite.** Headroom's own accounting: 5.2% of input tokens
  compressed ($0.07) — 40 of 116 requests had nothing compressible. This suite's contexts are
  2–4k tokens/request and its cost driver is TURNS, which compression cannot reduce; run-level
  $ differences are trajectory noise (the wrapped baseline drew expensive T6/T9 runs).
- **Where composition WOULD pay:** the proxy's single best hit (15.4k → 4.1k tokens, −73%) was
  a baseline whole-file read — the waste class Marksman eliminates at the source and Headroom
  compresses after the fact. On long sessions / big files (a `checker.ts` read is ~50k lines),
  a grep-and-read agent benefits from compression; a Marksman agent mostly never emits the
  dumps in the first place.

**The wrapped arm's "improvement," explained (transcript-diffed):** the wrapped rust runs
posted the lowest turn counts ever (2-turn renames, $0.027 T1) — and it was NOT compression.
Behind the proxy, Claude Code registered the marksman MCP tools **upfront**
(init: 34 tools, marksman `connected`, zero ToolSearch calls in all 10 transcripts — first
action is `apply_edits` directly); unwrapped, the tools are **deferred** (29 tools, marksman
`pending`) and every session pays a ToolSearch discovery turn first. The proxy's first-request
latency appears to let the MCP handshake win the client's registration race — marksman-mcp
itself answers `initialize` in 0.13s, so the deferral is client-side policy/timing, not the
server. Marksman's replies passed through the proxy byte-identical (the T1 tool_result is the
same 389 chars in both arms). Two implications: (1) wrapped-vs-unwrapped comparisons change
TWO variables (compression + tool-registration mode) — attribute with transcripts, not totals;
(2) **every published marksman number in this file INCLUDES a discovery turn** — a client that
registers MCP tools eagerly gets 2-turn renames (T1 at −83% vs baseline instead of −70%). The
discovery turn costs ~1 turn + ~2¢/session; eager registration is worth requesting from any
client that supports it.

Other caveats: single runs; `in_tok` is not comparable across wrapped/unwrapped arms (proxy
cache alignment shifts the cache-write/read mix). Recipe: `headroom proxy --port 8787`, a shim
exporting `ANTHROPIC_BASE_URL`, `CLAUDE_BIN=<shim> go.sh …` (go.sh honors a pre-set
`CLAUDE_BIN` and exports `CLAUDE_REAL` for the shim).

### Retrieval overlap vs the Node prototype (Jaccard, per task)

| task | rust | node | shared | Jaccard |
|---|--:|--:|--:|--:|
| merge bm25/vector/symbol with RRF | 18 | 18 | 14 | 64% |
| ast-anchored structural edits + gate | 14 | 13 | 9 | 50% |
| package-aware relevance weighting | 15 | 14 | 9 | 45% |
| import graph + seed expansion | 16 | 16 | 12 | 60% |

Mean ≈ 55% — honest moderate overlap with expected divergence: different embedder
(potion-code Model2Vec vs bge-small) and different graph (SCIP semantic references vs ts-morph
import declarations). The cores (BM25/RRF/weighting/expansion) are faithful ports.

### Multi-language retrieval (registry vs single-provider)

Mixed Rust+TS+Python fixture, six labeled tasks (`scripts/multilang-bench/`): single-provider
(`CI_LANG=rust`) retrieves **2/6** (non-Rust files are never indexed — recall 0 at any rank);
the extension→provider registry retrieves **6/6** (hit@5). Retrieval itself is unchanged and
language-blind; the gain is purely that every language's files make it *into* the one index.

### Edit capability (not a timing)

| | SCIP only | **SCIP + tree-sitter** | Node (ts-morph) |
|---|---|---|---|
| rename / refs / import graph | ✅ compiler-grade | ✅ | ✅ |
| whole-symbol edits (replace_node) | ✅ | ✅ | ✅ |
| move (importer rewrite) | ✅ via LSP willRenameFiles | ✅ | ✅ |
| **sub-symbol edits** (body/return/param) | ❌ no AST | **✅ tree-sitter** | ✅ |
| external runtime dep for the AST | — | **none (in-process)** | Node |

Edit-gate latency: the default write engine is **ts-morph** (in-process, kept warm) — a full
rename + blast-radius gate is **~0.9s** (vs a cold LSP server's ~68s; `CI_EDIT_ENGINE=lsp`
keeps the generic fallback, which is how the Rust gate drives rust-analyzer).

---

## 5. Reproduce

```bash
# The agent A/B (needs $ANTHROPIC_API_KEY; rebuilds release binaries first):
bash scripts/agent-bench/go.sh --runs 3
# One task:
bash scripts/agent-bench/go.sh --task T10-monorepo --runs 1
# The ablation arms:
CI_TS_MODE=treesitter-gated bash scripts/agent-bench/go.sh --arms rust --runs 1
CI_TS_MODE=treesitter       bash scripts/agent-bench/go.sh --arms rust --runs 1
# Where the turns went:
bash scripts/agent-bench/go.sh --task T5-schema-field --save-transcript /tmp/tx
python3 scripts/agent-bench/analyze.py /tmp/tx

# Micro-benchmarks:
python3 scripts/bench.py [oracle_repo]
python3 scripts/multilang-bench/run.py
```

Method notes: `--release` binaries; timing micro-benchmarks are `min of 3` after a discarded
warmup; agent runs reset git + restore a pristine base-consistent index snapshot before every
run; the fixture repos are copied to throwaway git repos per run.
