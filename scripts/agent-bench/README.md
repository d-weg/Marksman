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
   run contaminates the next.
4. **Tokens from the source of truth.** Counts come straight from Claude Code's own
   `--output-format json` (`usage`), so the baseline's grep/read/round-trips are counted in
   full — nothing is hand-waved or modeled.
5. **No cherry-picking.** Every task in `tasks.json` is reported, including ties and losses.
   Run with `--runs N` to median over N repetitions and damp model nondeterminism.
6. **One command, reproducible.** Anyone can re-run it and get the same shape of result.

## Run it

```bash
cargo build --release                       # build codeindex-rs + the MCP server
python3 scripts/agent-bench/run.py \
    --repo /path/to/a/typescript/repo \
    --runs 3
```

Requires the `claude` CLI on PATH. The script builds the index once, then for each task runs
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
| T3-locate-edit | flip one default value | the *find* cost (retrieve vs grep) |
