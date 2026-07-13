# Marksman

[![CI](https://github.com/d-weg/Marksman/actions/workflows/ci.yml/badge.svg)](https://github.com/d-weg/Marksman/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Precise code retrieval and type-checked edits for coding agents, over MCP.**

Marksman is a local-first [Model Context Protocol](https://modelcontextprotocol.io) server, written in Rust, that gives an AI coding agent a reliable way to work with a codebase:

- **Find** the exact code relevant to a task — compiler-accurate symbols and an import graph, fused with semantic and keyword search, returned as line-ranged references rather than whole files.
- **Change** it safely — structured edits (rename, move, replace, and more) applied atomically and **type-checked over their blast radius before they land**. A cross-file rename is one call, and nothing commits if it would introduce a type error.

Both capabilities are exposed through just **two MCP tools**: `inspect` to read, `apply_edits` to write.

## Why

Coding agents spend a large share of their tokens searching for context, and blind string edits routinely break builds. Marksman addresses both: retrieval hands the agent the right line ranges directly, and every edit is verified by the language's own compiler — across the changed files *and* the files that depend on them — before it is committed. When a wide change is rejected, the rejection enumerates every affected site with a ready-to-apply fix, so the type checker does the site-finding.

On a 10-task end-to-end agent benchmark, an agent using Marksman completed every task correctly at **~45% lower cost and ~42% less wall-clock time** than the same agent without it (a repo-wide type rename: 3 turns instead of 22). Methodology, ablations, and caveats: [docs/benchmarks.md](docs/benchmarks.md).

## Tools

| Tool | Purpose |
|---|---|
| `inspect` | Read and locate code. One tool, dispatched by `mode`: `search` (find code by concept or task description), `symbol` (name → addressable handle), `file` (a file's symbols and imports), `node` (one symbol's full source or a sub-node), `map` (directory/architecture overview). Runs entirely locally — no API calls. |
| `apply_edits` | Apply structured edits: `rename`, `move_file`, `replace_text`, `replace_node`, `set_body`, `add_parameter`, `add_symbol`, and more. Edits are atomic and type-checked over the blast radius; symbols are addressed by name, so a named target needs no lookup step first. |

Handles returned by `inspect` feed directly into `apply_edits`.

## Language support

| Tier | Languages | Retrieval | Edit gate |
|---|---|---|---|
| **Stable** | TypeScript, Rust | Compiler-accurate (SCIP) + AST | Type-checked over the blast radius by the language's own toolchain |
| **Experimental** | Java, PHP, Swift | AST + import graph | Type-checked (`javac` / PHPStan / `swift build`), cross-file rename via LSP — landed, not yet benchmark-validated |
| **Best-effort** | Python, JavaScript, Go, Ruby, C, C++ | AST + import graph | Syntax-gated only (a change that no longer parses is rejected); renames are verified by a repo-wide scan with suggested fixes for unreachable sites |

Every response is explicit about which guarantee it carries (`gated: true/false`) — Marksman never silently degrades a gate. Per-language details, toolchains, and status: [docs/languages/](docs/languages/).

The core is language-blind; adding a language starts with a tree-sitter grammar and a compiler gate, with a SCIP indexer as the maturity step for monorepos and cross-package analysis. See the [roadmap](docs/roadmap.md).

## How it works

- **Read path.** A per-language indexer (SCIP where available) is merged with in-process tree-sitter parsing for sub-symbol structure, embedded by a small native code-embedding model (no GPU, no embedding server), and indexed for hybrid retrieval: BM25 + vector search + symbol match, fused with Reciprocal Rank Fusion and expanded along the import graph.
- **Write path.** A persistent, warmed gate engine applies edits behind an in-memory virtual filesystem and diffs type diagnostics over the blast radius — the changed files plus their importers — against a pre-edit baseline. Only a clean diff lands.
- **Fast startup.** The index is cached and validated by a content-hash fingerprint of the source; a warm start on an unchanged repo takes ~0.1s. Any doubt triggers a reindex — never a stale load.
- **Reads stay current.** Symbol ranges are re-anchored against file content on every read, and committed edits are described back into the index immediately — a symbol you just added is visible to `inspect` without a reindex.

Architecture details: [docs/architecture.md](docs/architecture.md).

## Installation

Three steps to set up — build, index, register — plus a one-time **agent preamble** (step 4) so your coding agent actually reaches for the tools. Everything beyond the build is per-language and lazy: toolchains are only needed for the languages your repository actually contains.

### Prerequisites

- **Rust** (stable), to build Marksman — <https://rustup.rs>
- The embedding model (~65 MB) downloads itself on first `index` (see below for offline setups)
- *TypeScript repos:* Node 18+
- *Rust repos, for type-checked edits:* rust-analyzer (`rustup component add rust-analyzer`)

`marksman doctor <repo>` reports exactly what your repository needs, what is installed, and how to install anything missing. A missing toolchain disables only that language, with a clear report.

### 1. Build

```bash
git clone https://github.com/d-weg/Marksman.git
cd Marksman
cargo build --release   # → target/release/marksman (CLI) and marksman-mcp (MCP server)
```

### 2. Index your repository

```bash
target/release/marksman index /path/to/your/repo   # writes .marksman/
# optional sanity check:
target/release/marksman retrieve /path/to/your/repo "where is the rate limiter"
```

### 3. Register the MCP server

**Claude Code:**

```bash
claude mcp add marksman -- /absolute/path/to/Marksman/target/release/marksman-mcp
```

**Any MCP client:**

```json
{
  "mcpServers": {
    "marksman": {
      "command": "/absolute/path/to/Marksman/target/release/marksman-mcp"
    }
  }
}
```

The server operates on the repository it is launched in (its working directory); pass `--root /path/to/repo` or set `MARKSMAN_ROOT` to point it elsewhere.

### 4. Point your agent at the tools

Registering the server makes the tools *available* — it doesn't make an agent *use* them. Left to
its own devices a coding agent falls back on grep + manual edits, and you never see Marksman's
type-checked, single-call editing. Add the preamble below to your repo's `CLAUDE.md` (or `AGENTS.md`,
or your client's system prompt) so the agent reaches for the tools by default. This is the **exact
nudge our benchmark suites use** ([`scripts/agent-bench/run.py`](scripts/agent-bench/run.py)) — the
one the published numbers were measured with:

```text
You have marksman MCP tools. They are DEFERRED — load them FIRST, in ONE call, with their FULL names:
  ToolSearch  query="select:mcp__marksman__apply_edits,mcp__marksman__inspect"
What they do: apply_edits (ALL code edits — structural + surgical, type-checked before landing when the language has a checker; TRUST the reply — never re-verify by hand), inspect (ALL reads/locating — mode: search|symbol|file|node|map).
Then EDIT WITH THE TOOL, not grep+Edit: if the task NAMES the symbol, call apply_edits by name DIRECTLY — don't locate it first; if the task also gives the FILE, address as `file#name` (e.g. `src/http/retry.ts#parseResponse`) so it resolves in ONE call; else a bare name works and an ambiguous one just returns candidate ids to re-issue with. This holds even for a ONE-LINE change (change a default, fix a value) — use apply_edits replace_text by name; do NOT reach for Grep/Bash/Read+Edit for a small edit. It's verified server-side and needs no separate search.
File moves/renames: send the BARE move_file/rename as your FIRST action — no find/grep/read survey first, no helper edits alongside (in type-checked languages, imports, module declarations, and needed module files are all part of the one action; ungated replies say exactly what remains). The reply shows every line it rewrote — exactly what a survey would have found — and the gate rejects safely if anything is off.
```

<details>
<summary>Offline / air-gapped, or a custom model location</summary>

To place the model by hand, drop it at the default path and Marksman finds it with no configuration:

```bash
mkdir -p ~/.marksman/models/potion-code-16M
curl -fL --output-dir ~/.marksman/models/potion-code-16M \
  -O https://huggingface.co/minishlab/potion-code-16M/resolve/main/model.safetensors \
  -O https://huggingface.co/minishlab/potion-code-16M/resolve/main/tokenizer.json \
  -O https://huggingface.co/minishlab/potion-code-16M/resolve/main/config.json
```

To keep it elsewhere, point `CI_MODEL_DIR` at that directory. `CI_NO_MODEL_FETCH=1` disables the auto-download.
</details>

## Container mode (optional)

Instead of installing each language's toolchain on the host, run it from a per-language OCI image: one container runtime instead of N toolchains, at pinned versions, so a host toolchain upgrade can never silently change a verdict.

```bash
scripts/marksman-images.sh check          # runtime? images? pins in sync?
scripts/marksman-images.sh build ts rust  # build just what your repos need
CI_SANDBOX=oci marksman-mcp --root /path/to/repo
```

Container mode is strictly opt-in, and failures are loud — a missing runtime or image is reported, never papered over with a different toolchain. Full walkthrough: [docs/container-guide.md](docs/container-guide.md).

## CLI

```
marksman index    <repo>                              # build / refresh the index (.marksman/)
marksman retrieve <repo> "<task>" [--top N] [--json]  # query the index
marksman doctor   [<repo>]                            # per-language dependency report;
                                                      # exit 1 if unhealthy
```

## Configuration

Marksman works with no configuration. For non-default setups:

| Variable | Meaning | Default |
|---|---|---|
| `MARKSMAN_ROOT` | Repository root for the MCP server | current directory |
| `CI_MODEL_DIR` | Embedding model directory | `~/.marksman/models/potion-code-16M` |
| `CI_NO_MODEL_FETCH` | Disable the first-use model download | unset (fetch enabled) |
| `CI_SANDBOX` | `oci` runs each language's toolchain in its container | host toolchains |
| `CI_SANDBOX_RUNTIME` | OCI runtime CLI to use | first of `container`/`docker`/`podman`/`nerdctl` on PATH |
| `CI_GATE_TIMEOUT_SECS` | Wall-clock ceiling for a gate verdict; a timeout refuses the edit — it never passes or downgrades the gate | `600` |
| `CI_EDIT_ENGINE` | Force a TypeScript write-engine tier: `tsgo` · `tsmorph` · `lsp` | auto-selected |
| `CI_TSGO` | Path to a `tsgo` binary (TypeScript 7 native) — the fastest gate tier | unset (PATH is probed) |
| `CI_NPM_CACHE` | npm cache directory for `npx`-fetched tools | system temp |
| `CI_SCIP_<LANG>` | Enable/disable the SCIP indexer per language (`1`/`0`) | rust: on · others: config value |
| `CI_PROVIDER` | `sidecar` runs language providers as separate processes | in-process |

An optional `marksman.config.json` in the repository root overrides retrieval and index settings (top-N, fusion weights, per-language SCIP, …).

## Project status

Marksman is under active development. TypeScript and Rust are the reference implementations — fully gated and benchmark-validated. Java, PHP, and Swift have gated edits landed with benchmark validation in progress. The remaining languages ride the generic tree-sitter provider with syntax-level gating until their compiler gate lands.

The codebase is a 16-crate Rust workspace: a language-blind core (`ci-*` crates) plus per-language providers under `crates/langs/`, held to a zero-warning policy with unit tests in CI and real-toolchain integration suites run before provider changes. See [docs/](docs/) for architecture, the provider contract, roadmap, and benchmarks.

## Contributing

PRs welcome — [CONTRIBUTING.md](CONTRIBUTING.md) covers the build, the two test tiers, the benchmark rules, and the project's non-negotiables (never serve stale reads, never silently degrade a gate).

## License

MIT — see [LICENSE](LICENSE).
