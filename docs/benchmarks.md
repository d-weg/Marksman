# Marksman — benchmarks

This file answers, in order:

1. [Does the tool actually help an agent?](#1-does-it-help--the-live-agent-ab) — the headline A/B.
2. [Which parts of the design earn their keep?](#2-which-parts-of-the-design-earn-their-keep--the-read-path-ablation) — an ablation, and the language-rollout policy it settled.
3. [Does it hold up on a big real repo?](#3-does-it-hold-up-on-a-big-real-repo)
4. [How do the client and middleware change the numbers?](#4-how-the-client-and-middleware-change-the-numbers)
5. [What makes a tool response effective?](#5-tool-response-design-the-lesson-that-transfers) — the design lesson that generalizes beyond this project.
6. [Component micro-benchmarks](#6-component-micro-benchmarks).
7. [Methodology, trust properties, and how to reproduce](#7-methodology-trust-and-reproduction).

### Terms used throughout

- **baseline / rust** — the two *arms* of every A/B: the same agent (Claude Code headless,
  sonnet 4.6) with only its standard tools (baseline) vs with the Marksman MCP server loaded
  (rust). Nothing else differs.
- **turn** — one agent↔model round trip. Every turn re-sends the whole conversation, so
  **turns are the dominant cost driver**, not the size of any single response.
- **$** — Claude Code's own reported cost (`total_cost_usd`). The truest single score: it bakes
  in prompt caching and output pricing. Raw token counts (`in_tok`) can mislead — identical
  tokens can bill differently depending on cache hits — so read **turns** and **$**.
- **the gate** — before a Marksman edit lands on disk, the change is type-checked together
  with every file the change could break; if anything *new* breaks, nothing is written and the
  reply lists every affected site. Pre-existing errors never block (the gate diffs against a
  baseline).
- **blast radius** — that set of possibly-affected files: the changed files plus the files
  that import them.
- **import graph** — which file depends on which. Built two ways: **syntactic** (parse the
  import statements — done in-process by *tree-sitter*, a fast error-tolerant parser) or
  **semantic** (ask the compiler what actually references what — stored in a *SCIP* index,
  produced by tools like `scip-typescript`). The difference matters enormously; §2 measures it.
- **barrel** — an index file that re-exports others (`export * from "./x"`). Consumers import
  the barrel, not the defining file — which hides the real dependency from a syntactic graph.
- **bare specifier** — an import by package name (`import { X } from "@acme/core"`) rather
  than by relative path. A syntactic parser cannot tell it from a third-party dependency.
- **ungated / fallback tier** — languages without a wired-up compiler (Python, Go, Java, …)
  get tree-sitter reads and structural edits that are syntax-checked only; every reply says
  `gated: false` so the agent knows the edit was not type-verified.

All results below are single runs unless stated (historical 3-run medians showed the same
shape); dates are 2026-07-02.

---

## 1. Does it help? — the live-agent A/B

The experiment: give the same agent the same task on the same repo, with and without
Marksman, and check the result objectively (a shell command per task: greps + the project's
own type-checker — an outcome the agent can't fake). Ten tasks: six on a ~600-symbol
TypeScript repo, plus four purpose-built fixtures — a mixed Rust+TS repo (T7), Python+Go on
the ungated tier (T8), a barrel-heavy repo (T9), and a TypeScript workspace monorepo (T10).

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
| T8-fallback | Python + Go renames, ungated tier | 8 / 0.0967 | **6 / 0.0811** |
| T9-barrel | required field consumed through a barrel | 17 / 0.1485 | **7 / 0.1337** |
| T10-monorepo | required field consumed cross-package | 10 / 0.0869 | **5 / 0.0860** |

### Why it wins — three mechanisms

- **One call replaces N hand-edits.** A repo-wide rename is a single `apply_edits`: the
  server rewrites the definition and every reference, type-checks the result, and commits
  atomically. The baseline agent reads whole files, edits each site by hand, and iterates
  the type-checker — T6 is 3 turns vs 22.
- **The type-checker finds the affected sites, so the agent doesn't search.** For a change
  that breaks many places (add a required field — T5, T9, T10), the agent makes the anchor
  edit alone; the gate *rejects* it with **every** affected site, each shown with its current
  source and a ready-to-copy fix. One batch later it's done. No grep can miss a site, because
  the compiler enumerated them — and this works through barrels (T9) and across package
  boundaries (T10).
- **Languages without a compiler still come out ahead** (T8): structural edits honestly
  labeled "not type-verified," plus a server-side verification scan whose findings each carry
  a copy-paste-executable fix (see [§5](#5-tool-response-design-the-lesson-that-transfers)).

### Honest caveats

- Single runs; trajectory variance is real (a T5-shaped task occasionally pre-explores and
  lands at 7–9 turns instead of 4 — still well under baseline).
- The tool descriptions the agent sees are audited to contain **zero** benchmark-specific
  content (an early revision leaked task answers into description examples; those runs were
  discarded).
- Every arm passed every task, so the gate's *insurance* value — catching a broken edit — is
  not in these numbers. The measured win is efficiency.
- Absolute deltas belong to these repos and tasks; the *shape* (structural edits and
  wide-blast-radius changes are the blowouts) is what generalizes.

*Historical note:* the first 7-task run also carried the frozen Node.js prototype Marksman
was rewritten from, as a third arm. Marksman won or tied it on every task (−53% vs baseline
overall). Details, including a contamination incident that led to stricter arm isolation,
are in git history (`docs/benchmarks.md` @ 43d4caf).

---

## 2. Which parts of the design earn their keep? — the read-path ablation

Marksman's TypeScript support stacks three layers: tree-sitter parsing (fast, in-process,
syntactic), a SCIP index (compiler-accurate symbols and references), and the ts-morph
compiler gate on edits. Which layer produces §1's win? `CI_TS_MODE` swaps the read path so
the same suite isolates each one:

| mode | reads & import graph | edit gate |
|---|---|---|
| `full` (the default) | SCIP + tree-sitter | compiler |
| `treesitter-gated` | tree-sitter only (syntactic) | compiler (same one) |
| `treesitter` | tree-sitter only | **none** (syntax check only) |

Three findings, each one level deeper:

### 2.1 The compiler gate carries most of the value

| mode | $ | vs baseline | success | turns per task (T1…T10) |
|---|--:|--:|--:|---|
| `full` | 0.6648 | **−45%** | 10/10 | 3 3 3 3 4 3 4 6 7 5 |
| `treesitter-gated` | 0.7639 | **−36%** | 10/10 | 3 3 3 3 4 3 4 6 6 8 |

Identical turn counts on eight of ten tasks: on a single-package repo with direct imports,
**tree-sitter reads + the compiler gate deliver most of the win with zero startup
dependencies** (no Node.js needed until an edit). The whole SCIP premium (~10 points) is
concentrated in T9 and T10 — the two tasks built to stress the import graph. §2.3 and §2.4
explain why.

### 2.2 Without any gate, the tool *loses* on typed languages

Pure tree-sitter (no compiler anywhere): **$1.14, 8/9 — worse than having no tool.** T5
fails outright (nothing syntactic can enumerate the consequences of a type change — that is
the compiler's irreplaceable job) and T6 "passes" at 25 turns, double baseline cost. The
ungated tier is right **only** where the language has no usable checker (Python/Go — T8, where
it reaches −63% at its best), and wrong wherever a compiler exists but goes unused. A related
lesson: an honest "NOT type-verified" reply outperforms a confident false one — see 2.3.

### 2.3 The gate is only as good as its blast radius (barrels)

A syntactic import graph edges a consumer to the **barrel** it imports, not to the file that
defines the symbol — and a barrel itself never gets a type error when a required field is
added behind it. So a naive one-hop blast radius misses the consumers entirely, and the gate
can claim "type-checked clean" on an edit that broke them.

Today this cannot happen: syntactic graphs serve the gate the **transitive** closure of
importers (`ci_core::transitive_reverse_imports`), and an end-to-end test pins it. We know
the failure was real because T9 measured it before the fix landed:

| arm (pre-fix) | turns | $ | ok |
|---|--:|--:|:--:|
| baseline | 17 | 0.1939 | 1/1 |
| `full` | 6 | 0.1142 | 1/1 |
| `treesitter-gated` | **5** | **0.0945** | **0/1 — cheapest, fastest, and wrong** |

The cheapest arm shipped a broken repo while claiming it was type-checked — the worst
possible failure, and the reason "the radius must be sound" is now contract, not judgment
(see [provider-contract.md](provider-contract.md)). Post-fix, both modes tie on T9. SCIP's
graph never needed the fix: the compiler resolves the barrel, so the consumer's edge points
directly at the defining file.

### 2.4 The radius is only as good as its edges (monorepos) — where SCIP is load-bearing

Workspace monorepos import by **bare specifier** (`@acme/core`). A syntactic parser cannot
distinguish that from a third-party package, so it records **no edge at all** — and no
transitive closure can recover an edge that doesn't exist. Only a compiler can resolve the
workspace alias. Measured on T10:

| arm | turns | $ | ok |
|---|--:|--:|:--:|
| baseline | 9–10 | 0.087–0.152 | 1/1 |
| `full` (SCIP) | 5–6 | 0.086–0.106 | 1/1 — the cross-package sites arrive in the reject, −30% |
| `treesitter-gated` | 8–9 | 0.150–0.152 | 1/1 *only because the agent re-verified by hand* — at baseline cost |

Without SCIP the tool's monorepo advantage evaporates to zero (and an e2e proves a trusting
trajectory commits broken). With it, T10 runs T5's playbook across package boundaries.

### 2.5 What this settled — how languages roll out

Adopted policy (roadmap Batch 8), directly from the data:

1. **TypeScript and Rust keep SCIP permanently.** It's not speed — warm startup is ~0.1s
   either way — it's what keeps the gate *sound* on barrels and package boundaries without
   the agent hand-verifying. The ablation modes exist for measurement, not as configurations.
2. **New languages land as tree-sitter + the language's own compiler gate first** (−36% with
   zero startup dependencies is most of the win), with a SCIP indexer added later, when that
   language's users hit monorepos or large repos. The `treesitter-gated` provider is the
   template.
3. **The ungated tier is only for languages without a usable checker**, and its replies must
   keep saying so.

---

## 3. Does it hold up on a big real repo?

The fixtures are tiny by design, so we ran the machinery (no agent) on the TypeScript
compiler itself — 709 source files, **453k lines, 21,794 symbols**:

| what | measured |
|---|--:|
| cold full index (scip + embeddings + persist) | **29.5s** |
| warm provider startup (content-fingerprint check) | **~0.3s** |
| `retrieve` per query, end-to-end | **0.26–0.40s** |
| gated edit on a leaf file (warm) | **0.098s** |
| gated edit on a hub file (303 real importers) | ~20s |

- Retrieval quality held (a "union type narrowing" query led with `compiler/types.ts`'
  `UnionType`), and its O(n) scan is a non-issue at 22k chunks.
- **Gate latency scales with the true blast radius, not repo size** — ~0.1s plus ~65ms per
  file the edit can affect. The common case (leaf files) stays ~0.1s; a hub edit pays for
  genuinely re-checking 303 referencing files, still under one full `tsc` run.
- The repo also stress-tested §2's conclusion at scale: it imports through namespace barrels,
  so the syntactic graph sees **1** importer of `compiler/core.ts` where SCIP sees **465**
  real referencers — and the transitive closure a syntactic gate would need is **596 files
  (the whole repo) for any hub edit** (median inflation 1.0×, p99 298×). At 65ms/file that's
  a ~40s gate per hub edit without SCIP, vs a bounded true-referencer set with it. On
  barrel-architected repos at scale, SCIP keeps the gate sound *and* affordable.
- The test also found (and we fixed, with a regression test) a real bug: the atomic index
  save was destroying the co-located SCIP cache on every save, silently costing the next
  startup a full re-index.

Caveats: one machine, single runs; `npm ci` failed in the sandbox (harmless here — the
compiler's sources are self-contained — but dependency-heavy repos need a working install
before SCIP indexing); declaration files (`.d.ts`) have no syntactic import edges at all.

---

## 4. How the client and middleware change the numbers

Two findings about the *environment* around the tool, both measured by diffing full
transcripts — totals alone attributed them to the wrong cause.

### 4.1 The tool-loading turn: your MCP client may cost you a turn per session

MCP clients register a server's tools in one of two ways: **upfront** (tool definitions
present from the first request) or **deferred** (the agent must call a tool-search tool to
load them, spending its first turn on discovery). In every measurement above, Claude Code
deferred Marksman's tools — so **every Marksman number in this file includes one discovery
turn**. When a run happened to register the tools upfront, every task dropped a turn: renames
completed in **2 turns at $0.027** (T1: −83% vs baseline instead of −70%).

The server is not the cause — `marksman-mcp` answers `initialize` in 0.13s; registration mode
is client-side policy/timing. If your client supports eager MCP registration, use it: it's
worth about one turn and ~2¢ per session.

### 4.2 Token-compression middleware: compatible, and mostly redundant behind Marksman

We ran the full suite through [Headroom](https://github.com/headroomlabs-ai/headroom) (an
open-source proxy that compresses tool outputs before they reach the model), wrapped vs two
same-day controls:

- **No interference.** 20/20 wrapped tasks passed; Marksman's replies passed through
  byte-identical. Headroom's own content router *protects error outputs and skips small
  content* — which is exactly what Marksman emits (small, dense replies; error-anchored
  rejects with verbatim fixes). The two designs are structurally disjoint.
- **Nothing to compress.** Headroom's own telemetry: 5.2% of input tokens ($0.07) across the
  whole run; 40 of 116 requests had no compressible content. This suite's cost is *turns*,
  which no proxy can reduce.
- **Where it would pay:** its single best hit (−73%) was a baseline whole-file read — the
  waste class Marksman prevents from entering context in the first place. On a grep-and-read
  agent with big files and long sessions, compression helps; behind Marksman there's little
  left to compress. Complementary, not competing.
- **Methodology warning:** wrapping the proxy also flipped the client into upfront tool
  registration (§4.1) — two variables at once. The apparent "Headroom speedup" of the
  Marksman arm was entirely §4.1. Attribute middleware effects with transcripts, not totals.

Recipe: `headroom proxy --port 8787`; a shim that exports
`ANTHROPIC_BASE_URL=http://127.0.0.1:8787` and executes the real claude binary;
`CLAUDE_BIN=<shim> bash scripts/agent-bench/go.sh …` (the harness honors a pre-set
`CLAUDE_BIN` and exports `CLAUDE_REAL` for the shim to use).

---

## 5. Tool-response design: the lesson that transfers

The ungated tier (T8: a Python and a Go rename, no compiler) only started winning when the
tool's **responses** stopped delegating work back to the agent. Three iterations on the same
task:

| response design | rust $ | turns | outcome |
|---|--:|--:|---|
| v1: "…review or run the project's own checks" | 0.1183 | 10 | **loses to baseline (+24%)** — the agent pays for the tool, then re-verifies everything by hand anyway |
| v2: + a server-side rename scan (file:line evidence, "do NOT grep") | 0.1202 | 9 | −28%, but the agent burned 3 failed attempts fixing a flagged site — it addressed the site by its OLD (pre-rename) name |
| v3: + every finding carries a **verbatim-executable fix** (anchored to the post-rename symbol; only one value left to fill in) | **0.0622** | **5** | **−63%** |

The transferable law — the same mechanism behind the reject-driven flow in §1: **a tool
response that leaves the agent any "check it yourself" or "figure out the addressing" step is
a design bug.** Do the check server-side, inline the evidence, and make every suggested
follow-up copy-paste executable. Response tokens are cheap (they arrive once and cache);
turns are expensive (they re-send everything).

---

## 6. Component micro-benchmarks

Machine-level numbers for individual pieces (oracle repo = the ~600-symbol Node.js
`codeindex` prototype; timings are min-of-3 after a discarded warmup).

**Indexing speed** — the tree-sitter merge is free, and Rust indexes ~4× faster than the
prototype:

| variant | time |
|---|---|
| Rust · SCIP only (no tree-sitter) | 2.99s |
| **Rust · SCIP + tree-sitter** | **3.02s** (+1.2%) |
| Node prototype (bge embedder) | 12.68s |

**Startup cache** — a content-hash fingerprint (sources + config + pinned tool versions)
decides load-vs-reindex; hashes, not mtimes, so a `git checkout` still hits the cache:

| | cold (source changed) | warm (fingerprint match) |
|---|--:|--:|
| TS provider startup | ~26s (scip-typescript run) | **0.11s** |

Any doubt re-indexes: a stale read would be a correctness bug; a spurious re-index is only a
slow start.

**Retrieval overlap vs the prototype** (same queries, Jaccard overlap of returned files):
mean ≈ 55% across four tasks (64/50/45/60%) — honest moderate overlap with expected causes:
a different embedder (potion-code Model2Vec vs bge-small) and a different graph (SCIP
references vs ts-morph import declarations). The ranking cores (BM25/RRF/weighting) are
faithful ports.

**Multi-language indexing**: on a mixed Rust+TS+Python fixture with six labeled retrieval
tasks, a single-language provider finds 2/6 (files of other languages are never indexed at
all); the per-file provider registry finds 6/6. The gain is recall into one shared index —
the retrieval math is language-blind and unchanged.

**Edit capability & latency**: sub-symbol edits (function body / return type / one
parameter) require the tree-sitter AST — SCIP alone is symbol-level. The default write
engine (ts-morph, kept warm) completes a full rename + blast-radius type-check in **~0.9s**
on the oracle repo; the generic LSP engine (`CI_EDIT_ENGINE=lsp`, how the Rust gate drives
rust-analyzer) is the slower fallback.

---

## 7. Methodology, trust, and reproduction

What makes the agent A/B trustworthy (`scripts/agent-bench/`, see its README):

- **One variable.** Same model, same prompt, same repo start-state; the only difference
  between arms is whether the MCP server is loaded. `--strict-mcp-config` on every arm so no
  other locally-registered server can leak in (that leak happened once; those runs were
  discarded and the flag added).
- **Objective checks.** Every task passes/fails by a shell command (greps + the project's
  own type-checker), not by judgment.
- **Clean state per run.** `git reset --hard` plus restoring a pristine, base-consistent
  index snapshot before every run; fixtures are copied to throwaway repos.
- **Whole-run accounting.** Tokens come from Claude Code's own JSON; subagent spawning is
  disabled so the reported numbers cover everything; every task is reported, including
  losses (see §2.2, §5 v1).
- **No benchmark-tuned prompting.** Tool descriptions are audited for fixture names/values.

Benchmarking doubled as testing — three real product bugs were found by measurement and
fixed with regression tests: a same-file batch corruption in the edit engine, the LSP gate
being blind to files a batch creates, and the index save destroying the SCIP cache (§3).

```bash
# The agent A/B (needs $ANTHROPIC_API_KEY; rebuilds release binaries first):
bash scripts/agent-bench/go.sh --runs 3
bash scripts/agent-bench/go.sh --task T10-monorepo --runs 1

# The ablation arms (§2):
CI_TS_MODE=treesitter-gated bash scripts/agent-bench/go.sh --arms rust --runs 1
CI_TS_MODE=treesitter       bash scripts/agent-bench/go.sh --arms rust --runs 1

# See where the turns went (per-tool transcript summary):
bash scripts/agent-bench/go.sh --task T5-schema-field --save-transcript /tmp/tx
python3 scripts/agent-bench/analyze.py /tmp/tx

# Micro-benchmarks (§6):
python3 scripts/bench.py [oracle_repo]
python3 scripts/multilang-bench/run.py
```
