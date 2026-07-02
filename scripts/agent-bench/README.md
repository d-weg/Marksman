# Agent A/B benchmark — does codeindex actually help an agent?

The question: **on the same task, does an agent with codeindex spend fewer tokens (and
succeed at least as often) as the same agent without it?**

This measures the thing that matters — the agent, end to end — not a synthetic retrieval
metric. It's built to be trustworthy, not to look good.

## Why you can trust it

1. **One variable.** Both arms are the *same* model, *same* prompt, *same* repo at the
   *same* commit. The only difference is whether the codeindex MCP tools are on the
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

## Run it (step by step)

Three arms: **baseline** (no codeindex) · **rust** (codeindex-rs MCP) · **ts** (Node codeindex MCP).
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

Env knobs: `CODEINDEX_TS_DIR` (default `/Users/davi.vasconcelos/codeindex`) points at the Node
codeindex checkout used for the `ts` arm. A preflight aborts loudly if `claude` can't return
JSON — the run never silently reports zeros.

The script builds each arm's index once, then for each task runs
the agent twice (with / without codeindex), checks success, and prints a markdown table +
totals (input/output token deltas, success counts).

> Uses `--dangerously-skip-permissions` so the agent can edit unattended. That's safe **here**
> precisely because every run is sandboxed by `git reset --hard` — do not point it at a repo
> with uncommitted work.

## What it proves (and doesn't)

- **Proves:** the real token + success delta of giving *this* agent *these* tools on *these*
  tasks, fully accounted.
- **Doesn't prove:** generalization beyond the task set. So keep `tasks.json` honest — a mix
  of codeindex's strengths (cross-file rename, file move) *and* its neutral cases (a tiny
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

Two optional per-task fields power T7:

- **`fixture`** — a subdirectory of this script's dir (e.g. `fixture-multilang/`) holding a
  self-contained repo. The harness copies it to a throwaway temp dir and `git init`s it there
  (never git-resets a fixture in place — it lives inside this project's worktree), then builds
  indexes/snapshots exactly like the main `--repo`. Tasks with a `fixture` don't need `--repo`:
  `python3 run.py --task T7-multilang --arms baseline,rust` runs standalone.
- **`arms`** — restricts which arms run the task. T7 sets `["baseline", "rust"]`: the Node
  oracle is TypeScript-only, so a `ts` arm would measure a tool on a repo it can't index.

Latest results and analysis live in [docs/benchmarks.md](../../docs/benchmarks.md).
