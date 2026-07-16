# Marksman — benchmarks

This file answers, in order:

1. [Does the tool actually help an agent?](#1-does-it-help--the-suite-ab) — the headline A/B: six task identities, run identically on a TypeScript and a Rust codebase, under intent-level prompts.
2. [Which parts of the design earn their keep?](#2-which-parts-of-the-design-earn-their-keep--the-read-path-ablation) — an ablation, and the language-rollout policy it settled.
3. [Does it hold up on a big real repo?](#3-does-it-hold-up-on-a-big-real-repo)
4. [How do the client and middleware change the numbers?](#4-how-the-client-and-middleware-change-the-numbers)
5. [What makes a tool response effective?](#5-tool-response-design-the-lesson-that-transfers) — the design lesson that generalizes beyond this project.
6. [The TypeScript 7 (tsgo) measurements](#6-the-typescript-7-tsgo-measurements--gate-engine-index-producer-embedding) — gate engine, LSP-as-producer, and parallel embedding.
7. [Component micro-benchmarks](#7-component-micro-benchmarks).
8. [Methodology, trust properties, and how to reproduce](#8-methodology-trust-and-reproduction).

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

Headline results are **3-run medians** dated 2026-07-04; other sections state their own
dates and run counts.

---

## 1. Does it help? — the suite A/B

The experiment: give the same agent the same task on the same repo, with and without
Marksman, and check the result objectively (a shell command per task: greps + the project's
own type-checker — an outcome the agent can't fake). Six **task identities** — rename, move,
locate-edit, body-edit, schema-field, type-rename — each run against two same-shaped
codebases (`--suite ts` and `--suite rust`), so every number has a cross-language twin.

**The prompts are intent-level on purpose.** An earlier revision of this benchmark (now
[legacy-benchmark.md](legacy-benchmark.md)) named every target outright — the exact class,
field, and file — which measures the edit machinery at maximum directness but skips the part
of real sessions where the agent must *find* things. The suite prompts say what a user would
say: "The rank-fusion tail dampening constant in this codebase is too aggressive. **Find
it** and change its value…", "Move the tokenizer **module**…". The agent owns the whole loop
— locate, decide, edit, verify. That makes these margins smaller than the legacy table's and
more honest; **the two sets of numbers must not be compared against each other**. Checks are
also strict about intent: type-rename's checker requires prose mentions (a doc comment) to
follow the rename, which Marksman's rename now does in the same call.

| suite | arm | input tok | output tok | $ | vs baseline (in/out/$) |
|---|---|--:|--:|--:|---|
| ts | baseline | 1,321,353 | 10,258 | 0.6356 | — |
| ts | **Marksman** | **550,418** | **2,986** | **0.3004** | **−58% / −71% / −53%** |
| rust | baseline | 1,049,615 | 9,247 | 0.5227 | — |
| rust | **Marksman** | **578,405** | **4,069** | **0.3348** | **−45% / −56% / −36%** |

Per task (median API calls / median $, Marksman **bold** where it wins on $):

| task | ts baseline | ts Marksman | rust baseline | rust Marksman |
|---|--:|--:|--:|--:|
| rename | 9 / 0.1025 | **3 / 0.0402** | 7 / 0.0760 | **3 / 0.0409** |
| move | 11 / 0.1331 | **3 / 0.0416** | 10 / 0.1194 | **4 / 0.0712** |
| locate-edit | 7 / 0.0711 | **4 / 0.0539** | 6 / 0.0597 | **4 / 0.0531** |
| body-edit | 8 / 0.0705 | **5 / 0.0667** | 5 / 0.0471 | 5 / 0.0674 |
| schema-field | 11 / 0.1262 | **4 / 0.0570** | 8 / 0.0921 | **4 / 0.0599** |
| type-rename | 10 / 0.1322 | **3 / 0.0410** | 9 / 0.1284 | **3 / 0.0423** |

Reproducibility: an independent 3-run pass earlier the same day (before a `move_file`
description refinement) landed within a few points on every cell — same winners, same
losers, same shapes; the largest movement was move-rust itself (−36% → −40% $ after the
description spelled out the move's per-language completeness).

### Direction still pays — but wide prompts win anyway

The spread inside the table is itself a finding. When a task names its target (rename,
type-rename), the agent goes straight to one `apply_edits` — 3 calls, −49…−63% — the same
directness the legacy benchmark measured. When the prompt withholds the target
(locate-edit's "find it"), the agent spends a retrieval call first and the margin narrows.
So: **the tool benefits from precise prompt direction, and still saves tokens when it
doesn't get any** — the wide-prompt totals above are −36/−53% on cost with the agent doing
its own finding. The one honest exception is body-edit: a task so small (one inserted line)
that the marksman arm's tool schemas cost more than the baseline's whole grep-and-edit
trajectory; on the rust suite that's a real +43% loss, on ts a −5% squeak. Below roughly a
$0.05-baseline task there is nothing left to save.

### Why it wins — three mechanisms

- **One call replaces N hand-edits.** A repo-wide rename is a single `apply_edits`: the
  server rewrites the definition, every reference, *and doc-comment mentions* (through the
  same gate), type-checks the result, and commits atomically — type-rename is 3 calls vs
  8–10, with the checker demanding the prose update too.
- **The type-checker finds the affected sites, so the agent doesn't search.** For a change
  that breaks many places (schema-field), the agent makes the anchor edit alone; the gate
  *rejects* it with **every** affected site, each shown with its current source and a
  ready-to-copy fix. One batch later it's done — 4 calls vs 8–12.
- **Responses carry what the agent would otherwise re-derive.** Retrieval pointers inline
  single-line symbols (so a constant's type is visible before editing it); `list_anchors`
  leads with a file's import lines; commits echo the edited block as written; rejects carry
  the target's original extent. Every one of those lines exists because a benchmark
  transcript showed an agent paying a turn to fetch it
  ([§5](#5-tool-response-design-the-lesson-that-transfers) is the general law).

### Honest caveats

- One machine, one model (sonnet 4.6), 3 runs per cell; single-run trajectory variance is
  real (move especially swings between a direct 3-call run and a survey-first 5–6).
- **move-rust is the open gap** (4 calls / $0.071 vs move-ts's 3 / $0.042): agents trust the
  engine to rewrite TS imports but hedge on Rust's module system — they type insurance
  helper edits (harmless: the server no-ops redundant ones) and survey first. Spelling out
  the move's per-language completeness in the description measurably shrank it (5→4 calls,
  −17% $ between consecutive passes); `dryRun` is offered as a one-call survey substitute.
  The residual is agent prior, not tool capability — a bare `move_file` has been the
  complete Rust move since the movefix engine landed.
- The tool descriptions the agent sees are audited to contain **zero** benchmark-specific
  content; suite prompts and fixtures were built after that audit and keep to it.
- Every run passed its checker, so the gate's *insurance* value — catching a broken edit —
  is mostly not in these numbers (it appeared once during development: the cargo gate
  rejecting a hallucinated `f64` type that an earlier gate had waved through; that class is
  now regression-tested).
- Absolute deltas belong to these fixtures; the *shape* (structural edits and
  wide-blast-radius changes are the blowouts; sub-$0.05 tasks are a wash) is what
  generalizes.
- These numbers include a one-turn tool-discovery tax imposed by the client's deferred MCP
  registration ([§4.1](#41-the-tool-loading-turn-your-mcp-client-may-cost-you-a-turn-per-session)
  measured the upfront-registration headroom on the legacy suite).

*The previous headline benchmark* — ten tasks with fully-named targets, including the
multilang/ungated/barrel/monorepo fixtures — *lives in*
[legacy-benchmark.md](legacy-benchmark.md); its T7–T10 fixture tasks remain live and its
results are still the reference for §2's ablations.

### 1.6 New-language suites (Java · PHP · Swift) — **WIP**

The suite above (TS · Rust) is ported to three new **gated** languages. Each has a per-language
page with its provider status, required/optional toolchains, and a **preliminary** benchmark.
The first run surfaced three issues — a PHP gate bug (false-rejected cross-file edits), a Swift
benchmark-prompt bug (a degenerate schema-field cell), and Java's jdtls-absent rename fallback.
**All three are now understood and addressed in-tree** (the PHP gate fix is regression-tested; the
Swift prompt is corrected; Java's fallback has a shipped workaround and container-mode jdtls) — the
per-language tables stay flagged only because a clean multi-run rerun hasn't been captured yet, not
because a fix is outstanding:

| language | gate | rename engine | provider status & preliminary benchmark |
|---|---|---|---|
| Java | `javac` | jdtls (rename) · movefix (move) | [languages/java.md](languages/java.md) |
| PHP | PHPStan | phpactor (rename) · movefix (move) | [languages/php.md](languages/php.md) |
| Swift | `swift build` | sourcekit-lsp (bundled) | [languages/swift.md](languages/swift.md) |

These are **experimental** and not part of the headline result until a clean re-run lands.

---

## 2. Which parts of the design earn their keep? — the read-path ablation

Marksman's TypeScript support stacks three layers: tree-sitter parsing (fast, in-process,
syntactic), a SCIP index (compiler-accurate symbols and references), and the ts-morph
compiler gate on edits. Which layer produces the win? `CI_TS_MODE` swaps the read path so
the same suite isolates each one (measured on the
[legacy T1–T10 suite](legacy-benchmark.md), whose task ids the tables below use):

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
   zero startup dependencies is most of the win), with a SCIP indexer added later. The `treesitter-gated` provider is the
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
turn**. In the one full-suite run where the tools registered upfront (legacy suite), every
task dropped a turn — renames completed in **2 turns at $0.027** (T1: −83% vs baseline instead of −70%) —
and the suite-level advantage measured **−60%** ($0.59 vs $1.46, −61/−60/−59/−60 in/out/sec/$,
10/10; a fair within-run comparison, since both arms ran in the same environment). Read the
−60% with one caveat: that run's baseline also drew expensive trajectories ($1.46 vs
$1.01–1.14 in the controls), so the honest statement is **−45% with the discovery tax,
trending toward −60% without it** — a multi-run confirmation would pin it down.

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

The ungated tier (T8 in the [legacy suite](legacy-benchmark.md): a Python and a Go rename,
no compiler) only started winning when the tool's **responses** stopped delegating work back
to the agent. Three iterations on the same task:

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

## 6. The TypeScript 7 (tsgo) measurements — gate engine, index producer, embedding

TypeScript 7 ("tsgo") is Microsoft's native Go port of the TypeScript compiler; its language
server speaks LSP directly and serves requests across threads. Three questions, measured
2026-07-02/03 on the same 10-core machine as §3 (single runs unless noted):

### 6.1 tsgo as the gate engine — now the default tier

The write gate re-checks the blast radius on every edit, so warm-engine latency is the number
users feel. End-to-end through the full provider (scip index → `apply_edits` → gate) on a
hub-and-40-consumers fixture:

| engine | cold engine + breaking hub edit | warm clean edit |
|---|--:|--:|
| tsls (`typescript-language-server`) | 4.41s | 1.518s |
| ts-morph sidecar (previous default) | 0.80s | 0.034s |
| **tsgo** | 3.45s | **0.011s** |

- Verdicts are IDENTICAL across engines: the breaking edit rejects naming all 40/40
  consumers (same TS2554 diagnostics, zero noise), the clean edit commits, rollback intact.
  A real-tool parity test (`rename_parity_tsmorph_vs_tsgo`) additionally requires the same
  cross-file rename to leave byte-identical trees through both engines.
- tsls's 1.5s warm floor is structural: it lacks LSP pull diagnostics, so every gate pays a
  1.5s publish-silence settle. tsgo uses the pull path — no settle at all (~0.3ms per
  radius file warm vs the ~65ms/file behind §3's 20s hub-edit figure).
- ts-morph's original reason to be the default — faster than the old LSP — no longer holds;
  tsgo is ~3× faster warm even at fixture scale and the gap widens with radius size.
- **Engine order is now tsgo → ts-morph → tsls.** tsgo is auto-picked only when it costs no
  network (`CI_TSGO=/path/to/tsgo`, or `tsgo` on PATH); `CI_EDIT_ENGINE=tsgo|tsmorph|lsp`
  forces a tier. Note the TS7 RC npm package (`typescript@rc`) ships only `tsc` — the LSP
  binary lives in `@typescript/native-preview` until GA.
- Cold project load (a few seconds) is hidden by the existing background prewarm. Post-commit
  read freshness is preserved under LSP engines by a tree-sitter re-describe of the changed
  files (scip fidelity returns at the next reindex).

### 6.2 Can an LSP replace the SCIP indexer? — parity yes, scale no

`ci-lsp-index` builds a genuine SCIP index by sweeping a language server (`documentSymbol`
for definitions, `references` per symbol for occurrences), selectable as `CI_TS_MODE=lsp`.
The same read path consumes either producer, so the comparison is exact.

**Parity (fixtures + agent bench):** structure ids byte-equal on every file compared
(after two shape filters: object-literal members and import/re-export bindings are
references, not definitions), identical import graphs and gate verdicts, and the full agent
suite T1–T10 passes on the sweep index — including T9-barrel and T10-monorepo, the two tasks
built to break weaker indexes. T9 cost lands at scip parity (159k vs 189k input tokens,
single runs).

**Scale (microsoft/TypeScript src, 601 files / 379k lines / 22,160 symbols):**

| phase | time |
|---|--:|
| open + project load | 6.6s |
| documentSymbol + canonical-def filter | 5.2s |
| references sweep | **988s** |
| **sweep total** | **1000.8s** vs **scip-typescript 26.5s** — **38× slower** |

Per-symbol `references` cost is ~1ms on a 41-file fixture but **44.6ms at 379k lines** — the
per-query cost scales with PROJECT size, because an editor-oriented server (correctly)
maintains no whole-program reverse index. Asking it 22k times re-derives what a batch
indexer computes in one compiler pass; that asymmetry is *why indexers exist*.

**Verdict:** the sweep is the artifact producer for languages that have an LSP but **no**
SCIP indexer (where the honest alternative is nothing, and most such repos are far smaller
than 379k lines) — not a scip-typescript replacement. It also settles the planning-vs-editing
split: the read index answers many speculative O(1) planning queries from a loaded artifact;
the live engine answers the few targeted queries an edit needs. Each side keeps the tool
shaped for its access pattern.

### 6.3 Embedding parallelization — the warm-reindex bottleneck removed

Phase timing (`CI_TIMING=1`) showed embedding is ~75% of a warm reindex (10.6s of §3's
13.9s at 22k chunks). It is pure per-chunk CPU (Model2Vec static lookups) and shards
cleanly across scoped threads:

| threads | 22k chunks | speedup |
|--:|--:|--:|
| 1 | 10.58s | — |
| 2 | 5.26s | 2.01× |
| 6 | 1.99s | 5.31× |
| 8 | 1.89s | **5.61×** |

Near-linear on the 6 performance cores, CPU-bound (not memory-bound), byte-identical
output. Shipped in `build_index`/`update_index` via `std::thread::scope` (one flat map does
not earn a rayon dependency); projected warm reindex at §3 scale: 13.9s → ~5s.

## 7. Component micro-benchmarks

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
parameter) require the tree-sitter AST — SCIP alone is symbol-level. The ts-morph engine
(kept warm) completes a full rename + blast-radius type-check in **~0.9s** on the oracle
repo; §6.1 has the current engine tiers (tsgo → ts-morph → tsls) and their warm-gate
latencies.

---

## 8. Methodology, trust, and reproduction

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
# The suite A/B (§1 — needs $ANTHROPIC_API_KEY; rebuilds release binaries first):
bash scripts/agent-bench/go.sh --suite ts,rust --runs 3 --save-transcript /tmp/suites
# One task, one suite; or the legacy T-tasks (legacy-benchmark.md):
bash scripts/agent-bench/go.sh --task move --suite rust --runs 1
bash scripts/agent-bench/go.sh --task T10-monorepo --runs 1

# The ablation arms (§2):
CI_TS_MODE=treesitter-gated bash scripts/agent-bench/go.sh --arms rust --runs 1
CI_TS_MODE=treesitter       bash scripts/agent-bench/go.sh --arms rust --runs 1

# See where the turns went (per-tool transcript summary):
bash scripts/agent-bench/go.sh --task T5-schema-field --save-transcript /tmp/tx
python3 scripts/agent-bench/analyze.py /tmp/tx

# Micro-benchmarks (§7):
python3 scripts/bench.py [oracle_repo]
python3 scripts/multilang-bench/run.py
```
