# Marksman

[![CI](https://github.com/d-weg/Marksman/actions/workflows/ci.yml/badge.svg)](https://github.com/d-weg/Marksman/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Precise code retrieval and type-checked edits for coding agents — over MCP.**

Marksman is a local-first [Model Context Protocol](https://modelcontextprotocol.io) server that gives an AI coding agent two things grep-and-guess can't:

- **Find** the exact code for a task — compiler-accurate symbols + an import graph, fused with semantic and keyword search, returned as a **line-ranged manifest** (not a pile of whole files to read).
- **Change** it safely — structured edits (rename / move / replace) applied atomically and **type-checked over the blast radius before they land**. A cross-file rename is *one* call, not N hand-edits, and nothing commits if it would break the build.

Written in Rust: a language-blind core plus per-language providers. **TypeScript and Rust are fully type-checked; Python, Go, Java, Ruby, C, and C++ ride a generic in-process tree-sitter provider** (full retrieval + structural edits, honestly reported as un-type-checked).

## Why

Agents burn tokens grepping for context and break builds with blind string edits. Marksman hands the agent the *right* line-ranges and lets it make *type-checked* structural changes in one shot.

On a 7-task agent benchmark (Claude Sonnet, end-to-end, objectively checked), an agent **with** Marksman cost **~53% less and finished ~51% faster** than without, 7/7 correct — repo-wide type rename: **3 turns vs the baseline's 18**. The suite includes a **multilanguage task**: a Rust and a TypeScript rename in one session, each gated by its own compiler (4 turns vs baseline's 13). The tool descriptions are audited to contain zero benchmark-specific content. Details and honest caveats: [docs/benchmarks.md](docs/benchmarks.md).

## Capabilities (MCP tools)

| tool | what it does |
|---|---|
| `retrieve_context` | A line-ranged manifest of the code relevant to a task. Hybrid index (BM25 + Model2Vec embeddings + symbol match) fused with RRF and expanded along the import graph. No API calls. |
| `describe_architecture` | A per-directory file-kind / module map of the repo. |
| `find_symbols` | Exact/substring search over indexed symbol names → self-locating node-id handles that feed straight into `read_node` / `apply_edits`. |
| `list_anchors` | A file's symbols and sub-nodes (params / return / body) — the anchors you target with `apply_edits`. |
| `read_node` | The full source of one anchor (a symbol or its `:body` / `:param.N` / `:return` / `:doc` sub-node). |
| `apply_edits` | Structured edits (`rename`, `move_file`, `replace_text`, `replace_node`, `set_body`, `insert_member`, `insert_in_body`, `add_parameter`, …), applied atomically and **type-checked over the blast radius** — nothing lands if it introduces a new type error, including in files that import what changed. Symbols are addressed by name (ambiguity auto-resolves from the edit's own text where possible). For wide changes, a rejection lists **every** affected site with its source and a ready-to-copy fix — the type-checker is the site-finder. |

## How it works

- **Read:** `scip-typescript` (compiler-accurate symbols + cross-file references) merged with in-process **tree-sitter** (sub-symbol AST), embedded by a native **Model2Vec** (`potion-code-16M`) embedder — no GPU, no embedding server — then indexed (BM25 + flat vector store + import graph, persisted as protobuf) and retrieved with **Reciprocal Rank Fusion** + graph expansion.
- **Write:** a persistent, warmed **ts-morph** engine applies edits behind an in-memory VFS, gated by a baseline-diff of type diagnostics over the **blast radius** (the changed files + their importers). A generic LSP path is the fallback (`CI_EDIT_ENGINE=lsp`); it prefers **LSP 3.17 pull diagnostics** (request/response — a slow server can never be mistaken for a clean file), which is how the Rust gate drives rust-analyzer.
- **Fast startup:** the SCIP index is cached and validated by a content-hash fingerprint of the source (plus pinned tool versions) — a warm start on an unchanged repo is **~0.1s instead of ~26s**; any doubt reindexes, never a stale load.
- **Reads stay true in-session:** symbol ranges are re-anchored against the current file content on every read, and after a committed edit the write engine **re-describes the changed files** (new symbols, new import edges) back into the read path — a function you just added is immediately visible to `list_anchors`/`find_symbols`, no reindex. The Rust scip graph gets the same treatment: fingerprinted at build, with drifted/edited files served fresh tree-sitter edges.

## Install

### Prerequisites

Dependencies are **per language, checked only for languages your repo actually contains** — a
Rust-only repo never needs (or touches) Node, and a TS-only repo never needs rust-analyzer.
Run `marksman doctor <repo>` any time to see exactly what your repo needs, what's
installed, and how to install what's missing.

- **Rust** (stable) — to build Marksman itself. <https://rustup.rs>
- The **embedding model** (~65 MB), `minishlab/potion-code-16M` — downloaded once (below).
- *Only if your repo has TypeScript:* **Node 18+** with `npm`/`npx` — `scip-typescript` and
  `ts-morph` are then fetched automatically on first use.
- *Only if your repo has Rust and you want type-checked edits:* **rust-analyzer**
  (`rustup component add rust-analyzer`) — reads/indexing work without it.

If a needed toolchain is missing, Marksman says so **actionably** — the language is disabled
with an install instruction (at startup, in `doctor`, and on any tool call touching that
language's files) — it never half-works or silently degrades.

### 1. Build
```bash
git clone https://github.com/d-weg/Marksman.git
cd Marksman
cargo build --release
# produces: target/release/marksman (CLI) and target/release/marksman-mcp (MCP server)
```

### 2. Get the embedding model
Marksman uses a small static Model2Vec embedder. Download the model and point `CI_MODEL_DIR` at the directory:
```bash
# Option A — git-lfs
git lfs install
git clone https://huggingface.co/minishlab/potion-code-16M ~/.marksman/models/potion-code-16M

# Option B — Hugging Face CLI
# huggingface-cli download minishlab/potion-code-16M --local-dir ~/.marksman/models/potion-code-16M

export CI_MODEL_DIR="$HOME/.marksman/models/potion-code-16M"
```
The directory must contain `model.safetensors`, `tokenizer.json`, and `config.json`.

### 3. Index a repo
```bash
export CI_MODEL_DIR="$HOME/.marksman/models/potion-code-16M"
target/release/marksman index /path/to/your/ts-repo               # writes .marksman/ into the repo
target/release/marksman retrieve /path/to/your/ts-repo "where is the rate limiter"   # sanity check
```

### 4. Register the MCP server with your agent
Add Marksman to your MCP client's config (Claude Code, Cursor, or any MCP client). Generic form:
```json
{
  "mcpServers": {
    "marksman": {
      "command": "/absolute/path/to/Marksman/target/release/marksman-mcp",
      "env": {
        "CI_MODEL_DIR": "/home/you/.marksman/models/potion-code-16M",
        "CI_NPM_CACHE": "/tmp/ci-npm-cache"
      }
    }
  }
}
```
The server indexes the repo it is launched in (its working directory); or pass `--root /path/to/repo`, or set `MARKSMAN_ROOT`. Build the index once with `marksman index` (step 3) before first use.

For **Claude Code**:
```bash
claude mcp add marksman \
  --env CI_MODEL_DIR="$HOME/.marksman/models/potion-code-16M" \
  -- /absolute/path/to/Marksman/target/release/marksman-mcp
```

## CLI

```
marksman index    <repo>                              # build / refresh the index (.marksman/)
marksman retrieve <repo> "<task>" [--top N] [--json]  # query the index
marksman doctor   [<repo>]                            # per-language dependency report: what this
                                                          # repo needs, what's installed, what's missing
                                                          # (with install commands); exit 1 if unhealthy
```

## Configuration

Environment variables:

| var | meaning | default |
|---|---|---|
| `CI_MODEL_DIR` | Model2Vec model directory | **required** (set it) |
| `CI_NPM_CACHE` | npm cache dir for `npx` (scip / tsserver) | system temp |
| `CI_TSMORPH_DIR` | where the ts-morph sidecar is installed | `<tmp>/ci-tsmorph` |
| `CI_EDIT_ENGINE` | write engine: `tsmorph` (default) or `lsp` | `tsmorph` |
| `CI_PROVIDER` | `sidecar` runs the language provider as a separate process over a protobuf wire (`marksman-provider-<lang>`); unset = in-process | in-process |
| `CI_SCIP_<LANG>` | overrides the `scip.<lang>` config setting (`1`=on, `0`=off), e.g. `CI_SCIP_RUST` — Rust import graph from `rust-analyzer scip` (compiler-accurate `use` edges) vs `mod`-only; generated at index time (≈ a `cargo check`) and content-fingerprinted: files edited since serve fresh tree-sitter edges, and a cache without a fingerprint is refused rather than trusted stale | the `scip.<lang>` config value |
| `MARKSMAN_ROOT` | repo root for the MCP server (legacy `CODEINDEX_ROOT` still honored) | current directory |

An optional `marksman.config.json` in the repo root (the legacy `codeindex.config.json` name is still read) overrides retrieval / index settings (top-N, RRF k, weights, …). For example, `{ "scip": { "rust": true } }` builds the Rust import graph from `rust-analyzer scip` (compiler-accurate `use` edges) instead of the `mod`-only tree-sitter graph (`scip` is a per-language map; `CI_SCIP_RUST` overrides it per-run).

## Status

- **Languages:** **TypeScript** (`scip-typescript` + `ts-morph`) and **Rust** (in-process tree-sitter + rust-analyzer) — both with type-checked, blast-radius-gated edits. **Python, Go, Java, Ruby, C, and C++** ride the generic in-process tree-sitter provider: full retrieval + skeletal outline + structural edits, but *ungated* (`gated: false`) until a language's LSP/indexer lands. Adding a fallback language is a grammar dependency plus a few table rows; upgrading one to gated is a new provider implementing the same `LanguageProvider` trait. The core (`ci-*` crates) is language-blind.
- 16-crate Rust workspace, ~50 unit tests plus real-tool integration tests. See [docs/](docs/) for architecture, roadmap, and benchmarks.

## Contributing

PRs welcome — [CONTRIBUTING.md](CONTRIBUTING.md) covers the build, the two test tiers (fast
unit tests in CI; real-tool e2e suites to run locally before provider/gate changes), the
benchmark rules, and the project's non-negotiables (never serve stale reads, never silently
degrade a gate).

## License

MIT — see [LICENSE](LICENSE).
