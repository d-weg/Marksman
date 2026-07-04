# Marksman — the legacy benchmark (T1–T10, direct prompts)

This is the original headline A/B, preserved verbatim as a historical result. It has been
superseded as the primary benchmark by the **suite benchmark** in
[benchmarks.md §1](benchmarks.md#1-does-it-help--the-suite-ab) — read this file for what it
is: the measurement of the tool's *mechanics* under maximally direct prompts.

**Why "legacy", and why the numbers are not comparable to the suite:** these task prompts
hand the agent the answer's location. T3 names the exact class and field ("In the BM25
class, change the default value of the k1 field"); T2 frames the move as a file operation
with both paths spelled out; T4 names the function, the file, and quotes the exact strings
to change. Under those prompts the agent skips all research — the marksman arm's typical
trajectory is `ToolSearch → apply_edits`, and retrieval is never called. That isolates the
edit machinery beautifully, but it measures a *narrower* loop than real sessions run. The
suite benchmark widens the prompts to intent level ("Find it and change its value…", "Move
the tokenizer module…") so the agent owns locate → decide → edit → verify; its absolute
margins are smaller and *more honest*. Comparing a suite number against a T-number reads as
a regression when it is actually the benchmark getting stricter — don't.

Terms (baseline/rust arms, turns, $, the gate, blast radius) are defined in
[benchmarks.md](benchmarks.md#terms-used-throughout). All results below are single runs
unless stated (historical 3-run medians showed the same shape); dates are 2026-07-02.

---

## The legacy A/B

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
  a copy-paste-executable fix (see
  [benchmarks.md §5](benchmarks.md#5-tool-response-design-the-lesson-that-transfers)).

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
- These numbers include a one-turn tool-discovery tax imposed by the client's deferred MCP
  registration; the same suite measured **−60%** when the tools registered upfront
  ([benchmarks.md §4.1](benchmarks.md#41-the-tool-loading-turn-your-mcp-client-may-cost-you-a-turn-per-session)).

*Historical note:* the first 7-task run also carried the frozen Node.js prototype Marksman
was rewritten from, as a third arm. Marksman won or tied it on every task (−53% vs baseline
overall). Details, including a contamination incident that led to stricter arm isolation,
are in git history (`docs/benchmarks.md` @ 43d4caf).

The T7–T10 fixture tasks remain live in `scripts/agent-bench/tasks.json` — they measure
capabilities (multi-language sessions, the ungated tier, barrels, monorepos) that the suite
benchmark's six identities don't cover, and the ablation results in
[benchmarks.md §2](benchmarks.md#2-which-parts-of-the-design-earn-their-keep--the-read-path-ablation)
are stated in their terms.
