# Contributing to Marksman

Thanks for looking under the hood. This page is the practical map: how to build, test,
benchmark, and get a change merged.

## Build

```bash
cargo build --workspace            # debug
cargo build --release              # the binaries the MCP config and benches use
```

Prerequisites beyond Rust stable: **Node 18+** (`npm`/`npx`) for the TypeScript provider's
tooling (`scip-typescript`, `ts-morph` — fetched automatically on first use), and
**rust-analyzer** (`rustup component add rust-analyzer`) for the Rust provider's edit gate.
Neither is needed to compile — only to run the e2e tests and the tool itself against real repos.

## Tests — two tiers

```bash
cargo test --workspace             # fast: pure-Rust unit tests, no subprocesses (CI runs this)
cargo test -p lang-ts   -- --ignored   # e2e: real scip-typescript + node + ts-morph
cargo test -p lang-rust -- --ignored   # e2e: real rust-analyzer (try --test-threads=2: the
                                       # gate must hold under CPU contention)
cargo test -p ci-lsp    -- --ignored   # e2e: real typescript-language-server
```

The `#[ignore]` tier spawns the real external tools and needs network on first run (npm
fetches). **Run the relevant e2e suite before sending a PR that touches a provider, the edit
gate, or ci-lsp** — the unit tier deliberately cannot see integration regressions there.

House rules encoded in the tests, worth knowing before you change behavior:

- **Never serve stale reads.** Indexes are fingerprinted; any doubt reindexes or refuses —
  a wrong answer is worse than a slow one.
- **Never silently degrade.** A provider that can't gate (type-check) an edit must say so in
  the tool response (`gated: false`), not pretend.
- **The gate must be race-free.** Diagnostics are pulled (request/response) where the server
  supports it; don't reintroduce settle-on-silence heuristics.

## Benchmarks

- `python3 scripts/bench.py` — indexing/retrieval micro-benchmarks.
- `scripts/agent-bench/` — the live-agent A/B benchmark (costs real API money; see its README
  for the trust properties). If you add a task, keep the checks objective and **grep the tool
  descriptions for your fixture's names first** — benchmark-specific prompting invalidates the
  whole suite.

Results live in [docs/benchmarks.md](docs/benchmarks.md); update them only from a run you can
reproduce, and keep the honesty notes (single-run caveats, contamination, arm scope) intact.

## Style

- `rustfmt` is **not** enforced repo-wide (the codebase predates it; a blanket reformat would
  destroy blame). Match the style of the file you're in — notably its comment density: comments
  state constraints and *why*s, not what the next line does.
- `cargo clippy --workspace` should not gain new warnings from your change.
- Commit messages: imperative subject, body explains the *why* and the failure mode fixed.

## PRs

1. One logical change per PR; include the test that fails without it when fixing a bug.
2. Say which test tiers you ran (unit / which e2e suites).
3. Docs move with behavior: `docs/architecture.md` describes the current architecture — if your PR
   changes an invariant described there, update it in the same PR.

## Architecture orientation

Start with [docs/architecture.md](docs/architecture.md) (current design + invariants), then
[docs/roadmap.md](docs/roadmap.md). The one-paragraph version: a language-blind Rust core
(retrieval, index, VFS, gated edits) behind a `LanguageProvider` trait; each language crate
(`crates/langs/lang-*`) owns all of its language's external tooling; `ci-mcp` exposes the whole
thing over MCP and dispatches each edit to its file's provider.
