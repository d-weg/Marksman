# Agent A/B benchmark — does Marksman actually help an agent?

The question: **on the same task, does an agent with Marksman spend fewer tokens (and
succeed at least as often) as the same agent without it?**

This measures the thing that matters — the agent, end to end — not a synthetic retrieval
metric. It's built to be trustworthy, not to look good.

## Why you can trust it

1. **One variable.** Both arms are the *same* model, *same* prompt, *same* repo at the
   *same* commit. The only difference is whether the marksman MCP tools are on the
   allow-list. (Tools you don't use cost nothing, so this is a fair "availability" test.)
2. **Objective success.** Each task has a `check` command that exits 0/1 — a cross-file
   rename is "no old name left + new name present + still type-checks", not a vibe. No human
   grades the output.
3. **Clean slate every run.** `git reset --hard` + `git clean` before each agent run, so no
   run contaminates the next. Crucially this includes the **index**: `apply_edits` reindexes
   on commit, so a freshly built, base-consistent index is *snapshotted once and restored
   before every run* — otherwise an earlier run's edits would leave the next run searching a
   stale index that no longer matches the reset source.
4. **Tokens from the source of truth.** Counts come straight from Claude Code's own
   `--output-format json` (`usage`), so the baseline's grep/read/round-trips are counted in
   full — nothing is hand-waved or modeled.
5. **No cherry-picking.** Every task in `tasks.json` is reported, including ties and losses.
   Run with `--runs N` to median over N repetitions and damp model nondeterminism.
6. **One command, reproducible.** Anyone can re-run it and get the same shape of result.
   `go.sh` rebuilds the release binaries first, so a run can never measure a stale build.
7. **No benchmark-tuned prompting.** The MCP tool descriptions and the preamble contain **zero
   fixture names or task values** — every example uses identifiers verified absent from the
   target repo. (An earlier revision leaked near-verbatim task answers into description
   examples; those runs were discarded.) When adding an example to a tool description, grep the
   fixture for it first.
8. **Hermetic MCP config.** Every arm (baseline included) runs with `--strict-mcp-config`: the
   generated per-arm config is the ONLY MCP source, so user-scope servers registered on the
   bench machine can't leak tools into a run. (Caught in practice: a ts-arm task once picked up
   this repo's own globally-registered server mid-run.)

## Run it (step by step)

Arms: **baseline** (no Marksman) · **rust** (marksman MCP) — the comparison that matters.
A third **ts** arm (the Node codeindex MCP — the frozen, unmaintained prototype Marksman
rewrote) is opt-in via `--arms baseline,rust,ts`, kept only for historical comparison.
Each arm is a single agent — the subagent-spawn tool is disallowed, so the top-level token/cost
numbers describe the whole run (a delegated run would otherwise report the main agent's tokens while
`$` billed the hidden subagent too).

```bash
# 1. Auth — headless Claude Code needs an API key (org policy disables subscription headless).
export ANTHROPIC_API_KEY=sk-ant-...

# 2. A working claude CLI. If `claude` on PATH is a broken stub, point at the real binary:
export CLAUDE_BIN="/Users/<you>/Library/Application Support/Claude/claude-code/<ver>/claude.app/Contents/MacOS/claude"
#    (an API key makes the bundled binary work headless; or install the standalone CLI.)

# 3. Build the Rust tool.
cargo build --release

# 4. A DISPOSABLE clone of a TS repo with a clean git tree (the harness does git reset --hard).
git clone /path/to/some-ts-repo /tmp/bench-target
ln -s /path/to/some-ts-repo/node_modules /tmp/bench-target/node_modules   # so tsc/checks resolve

# 5. Run. The harness builds both indexes, then runs every task in every arm.
python3 scripts/agent-bench/run.py --repo /tmp/bench-target --runs 3
#    options: --arms baseline,rust   (drop the TS arm) · --task T1-rename · --runs N
```

Env knobs: `CODEINDEX_TS_DIR` (default `~/codeindex`) points at the Node codeindex checkout
used for the `ts` arm (which is **TypeScript-only** — it sits out T7-multilang). MCP configs
are generated at runtime from this checkout's paths, and every arm runs with
`--strict-mcp-config` so user-scope MCP servers on the machine can never leak into a run. A
preflight aborts loudly if `claude` can't return JSON — the run never silently reports zeros.

The script builds each arm's index once, then for each task runs
the agent twice (with / without Marksman), checks success, and prints a markdown table +
totals (input/output token deltas, success counts).

> Uses `--dangerously-skip-permissions` so the agent can edit unattended. That's safe **here**
> precisely because every run is sandboxed by `git reset --hard` — do not point it at a repo
> with uncommitted work.

## What it proves (and doesn't)

- **Proves:** the real token + success delta of giving *this* agent *these* tools on *these*
  tasks, fully accounted.
- **Doesn't prove:** generalization beyond the task set. So keep `tasks.json` honest — a mix
  of Marksman's strengths (cross-file rename, file move) *and* its neutral cases (a tiny
  one-line edit), so the average isn't stacked. Add your own tasks; the harness doesn't care
  what they are as long as the `check` is objective.

## Tasks (`tasks.json`)

| id | what | exercises |
|---|---|---|
| T1-rename | rename a function repo-wide | `apply_edits` rename vs N manual edits |
| T2-move | move a file + fix imports | `move_file` / willRenameFiles |
| T3-locate-edit | flip one default value | the *find* cost (edit-by-name + oldText disambiguation vs grep) |
| T4-body-edit | change two length checks inside one function | surgical sub-symbol edits (`replace_text`) vs read + re-emit |
| T5-schema-field | add a required field to an interface + set it at every construction site | the wide-blast-radius protocol: anchor edit → gate reject enumerates every site with ready-to-copy fixes → one batch |
| T6-type-rename | rename an interface repo-wide (definition + all references/imports) | gated cross-file rename at type level — the biggest baseline blowout (3 turns vs ~21) |
| T7-multilang | two renames in ONE session: a Rust function (cargo-checked) and a TS function (tsc-checked), in a mixed Rust+TS+Python repo | per-file provider dispatch — each edit gated by its own language's compiler; the multi-provider registry end to end |
| T8-fallback | two renames in ONE session: a Python and a Go function — neither language has a native integration | the GENERIC tree-sitter fallback provider (ungated edits, honestly labeled) — does structure-aware editing without a compiler gate still beat grep+Edit? |

Two optional per-task fields power T7:

- **`fixture`** — a subdirectory of this script's dir (e.g. `fixture-multilang/`) holding a
  self-contained repo. The harness copies it to a throwaway temp dir and `git init`s it there
  (never git-resets a fixture in place — it lives inside this project's worktree), then builds
  indexes/snapshots exactly like the main `--repo`. Tasks with a `fixture` don't need `--repo`:
  `python3 run.py --task T7-multilang --arms baseline,rust` runs standalone.
- **`arms`** — restricts which arms run the task. T7 sets `["baseline", "rust"]`: the Node
  oracle is TypeScript-only, so a `ts` arm would measure a tool on a repo it can't index.

## Transcript format — read the `.calls.jsonl`, not the raw stream

`--save-transcript` writes two files per run: the CLI's raw `stream-json` (`….jsonl`, ground
truth, grep it for tool inputs/outputs) and a normalized `….calls.jsonl` sidecar — ONE record
per API call, deduped, plus a trailer with the API's authoritative totals and cost.

**The raw stream is an accounting TRAP**: an assistant message streams as multiple events
(one per content block), each repeating the SAME cumulative input usage — naive summing
triple-counted a bench run's tokens before this sidecar existed. Any token/cost analysis must
read `.calls.jsonl` (or the trailer). Per-call `output` in the sidecar is a lower bound
(chunks snapshot it mid-message); the trailer's `usage_total.output` is exact.

`python3 run.py --normalize <dir>` retrofits sidecars onto existing transcript dirs.

## Suite-parameterized tasks (the convention)

The six basic tasks have ONE identity each — `rename`, `move`, `locate-edit`, `body-edit`,
`schema-field`, `type-rename` — and `--suite` points them at a language's fixture. Same
task, different repo:

```bash
python3 run.py --task schema-field --suite rust     # rustc as the gate
python3 run.py --task schema-field --suite ts       # tsc as the gate
python3 run.py --task rename --suite ts,rust        # both, in one invocation
python3 run.py --list-tasks                         # ids + available suites
```

Every suite is a port of the SAME corpus codebase (tokenize / store / rank / query / dedupe
over a doc-entry table) — `fixture-ts/` and `fixture-rust/` today — so cross-language numbers
compare by construction: the only variables are the language, its compiler, and the provider
behind it. Reports and transcripts use the expanded id (`schema-field-rust`).

The schema-field task is each suite's gate exercise: a REQUIRED field on a type constructed
in two files, so the compiler fails until every construction site is updated — ungameable.
Checkers are verified both ways before a suite lands: every check FAILS on the untouched
fixture, and a hand-applied reference solution passes.

**Promoting a language to the gated tier ships its suite**: port the corpus fixture, add a
`suites.<lang>` binding (fixture + language-native prompt/check) to each of the six tasks,
verify checkers both ways. (T1–T6 remain the legacy `--repo` TS tasks against the
Node-prototype checkout — the published §1 numbers are tied to them.)

## The tool surface: two tools

The server exports TWO tools — `apply_edits` plus one mode-dispatched `inspect`
(`search|symbol|file|node|map`). Settled by a controlled same-day A/B against the original
six-tool surface (3 runs x 12 task-suite cells, 2026-07-05): identical trajectories in every
cell, **−11.5% cost**, cheaper in 11 of 12 cells, zero native-tool usage in 36/36 runs.
The six-tool surface was removed rather than kept as a knob — settled experiments live on
the `research/full-surface` branch, not in the product.

## Other models (GLM, MiniMax, …) through the same harness

Several providers ship **Anthropic-compatible endpoints built to drive Claude Code** — which
means the whole bench runs unchanged against their models: same tasks, same objective checks,
same `.calls.jsonl` accounting; the only variable is the model. The mechanism is the same
`CLAUDE_BIN` shim the middleware experiment used (docs/benchmarks.md §4.2):

```bash
cat > /tmp/claude-glm <<'SH'
#!/bin/sh
export ANTHROPIC_BASE_URL="https://api.z.ai/api/anthropic"   # GLM (per z.ai docs)
export ANTHROPIC_AUTH_TOKEN="$GLM_API_KEY"
exec "${CLAUDE_REAL:-claude}" "$@"
SH
chmod +x /tmp/claude-glm

CLAUDE_BIN=/tmp/claude-glm CLAUDE_MODEL=glm-4.6 \
CI_BENCH_PRICE="in=0.6,out=2.2" \
bash scripts/agent-bench/go.sh --suite ts,rust --runs 3 --save-transcript /tmp/glm-suites
```

(MiniMax is the same shape with `https://api.minimax.io/anthropic` and its model id — check
each provider's current endpoint/model/pricing before a run.)

Two accounting rules for cross-provider tables:

- **`$` needs `CI_BENCH_PRICE`.** The CLI prices runs against Anthropic's own table, so its
  `total_cost_usd` is wrong or zero behind a third-party base URL. Set
  `CI_BENCH_PRICE="in=<$/MTok>,out=<$/MTok>[,cache_read=…][,cache_write=…]"` to recompute
  from the run's token counts — or ignore `$` and read tokens/turns, which are always true.
- **Compare arms within a model, never $ across models.** The claim worth testing is "does
  Marksman's delta hold on model X", not "is model X cheaper" — each model gets its own
  baseline.

Different *harnesses* (OpenCode, …) are a separate axis: different tool loop, upfront MCP
registration (no ToolSearch discovery turn), own transcript format — that needs a runner
backend, not a shim.

Latest results and analysis live in [docs/benchmarks.md](../../docs/benchmarks.md).
