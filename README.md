# Marksman

**Precise code retrieval and type-checked edits for coding agents — over MCP.**

Marksman is a local-first [Model Context Protocol](https://modelcontextprotocol.io) server that gives an AI coding agent two things grep-and-guess can't:

- **Find** the exact code for a task — compiler-accurate symbols + an import graph, fused with semantic and keyword search, returned as a **line-ranged manifest** (not a pile of whole files to read).
- **Change** it safely — structured edits (rename / move / replace) applied atomically and **type-checked over the blast radius before they land**. A cross-file rename is *one* call, not N hand-edits, and nothing commits if it would break the build.

Written in Rust: a language-blind core plus per-language providers. **TypeScript and Rust are fully type-checked; Python is supported via an in-process tree-sitter fallback (ungated edits).**

## Why

Agents burn tokens grepping for context and break builds with blind string edits. Marksman hands the agent the *right* line-ranges and lets it make *type-checked* structural changes in one shot.

On a 3-task agent benchmark (median of 3, Claude Sonnet), an agent **with** Marksman used **~39% fewer tokens and finished ~38% faster** than without — and edged out the mature TypeScript tool it's a rewrite of. Details and honest caveats: [docs/benchmarks.md](docs/benchmarks.md).

## Capabilities (MCP tools)

| tool | what it does |
|---|---|
| `retrieve_context` | A line-ranged manifest of the code relevant to a task. Hybrid index (BM25 + Model2Vec embeddings + symbol match) fused with RRF and expanded along the import graph. No API calls. |
| `describe_architecture` | A per-directory file-kind / module map of the repo. |
| `list_anchors` | A file's symbols and sub-nodes (params / return / body) — the anchors you target with `apply_edits`. |
| `apply_edits` | Structured edits (`rename`, `replace_node`, `insert_before`, `move_file`, `create_file`, `delete_file`), applied atomically and **type-checked over the blast radius** — nothing lands if it introduces a new type error, including in files that import what changed. |

## How it works

- **Read:** `scip-typescript` (compiler-accurate symbols + cross-file references) merged with in-process **tree-sitter** (sub-symbol AST), embedded by a native **Model2Vec** (`potion-code-16M`) embedder — no GPU, no embedding server — then indexed (BM25 + flat vector store + import graph) and retrieved with **Reciprocal Rank Fusion** + graph expansion.
- **Write:** a persistent, warmed **ts-morph** engine applies edits behind an in-memory VFS, gated by a baseline-diff of type diagnostics over the **blast radius** (the changed files + their importers). A generic LSP path is the fallback (`CI_EDIT_ENGINE=lsp`).

## Install

### Prerequisites
- **Rust** (stable) — to build. <https://rustup.rs>
- **Node 18+** with `npm` / `npx` — the TypeScript provider runs `scip-typescript` and `ts-morph`, fetched automatically on first use.
- The **embedding model** (~65 MB), `minishlab/potion-code-16M` — downloaded once (below).

### 1. Build
```bash
git clone https://github.com/d-weg/Marksman.git
cd Marksman
cargo build --release
# produces: target/release/codeindex-rs (CLI) and target/release/codeindex-rs-mcp (MCP server)
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
target/release/codeindex-rs index /path/to/your/ts-repo            # writes .codeindex-rs/ into the repo
target/release/codeindex-rs retrieve /path/to/your/ts-repo "where is the rate limiter"   # sanity check
```

### 4. Register the MCP server with your agent
Add Marksman to your MCP client's config (Claude Code, Cursor, or any MCP client). Generic form:
```json
{
  "mcpServers": {
    "marksman": {
      "command": "/absolute/path/to/Marksman/target/release/codeindex-rs-mcp",
      "env": {
        "CI_MODEL_DIR": "/home/you/.marksman/models/potion-code-16M",
        "CI_NPM_CACHE": "/tmp/ci-npm-cache"
      }
    }
  }
}
```
The server indexes the repo it is launched in (its working directory); or pass `--root /path/to/repo`, or set `CODEINDEX_ROOT`. Build the index once with `codeindex-rs index` (step 3) before first use.

For **Claude Code**:
```bash
claude mcp add marksman \
  --env CI_MODEL_DIR="$HOME/.marksman/models/potion-code-16M" \
  -- /absolute/path/to/Marksman/target/release/codeindex-rs-mcp
```

## CLI

```
codeindex-rs index    <repo>                              # build / refresh the index (.codeindex-rs/)
codeindex-rs retrieve <repo> "<task>" [--top N] [--json]  # query the index
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
| `CI_SCIP_<LANG>` | overrides the `scip.<lang>` config setting (`1`=on, `0`=off), e.g. `CI_SCIP_RUST` — Rust import graph from `rust-analyzer scip` (compiler-accurate `use` edges) vs `mod`-only; generated at index time, ≈ a `cargo check` | the `scip.<lang>` config value |
| `CODEINDEX_ROOT` | repo root for the MCP server | current directory |

An optional `codeindex.config.json` in the repo root overrides retrieval / index settings (top-N, RRF k, weights, …). For example, `{ "scip": { "rust": true } }` builds the Rust import graph from `rust-analyzer scip` (compiler-accurate `use` edges) instead of the `mod`-only tree-sitter graph (`scip` is a per-language map; `CI_SCIP_RUST` overrides it per-run).

## Status

- **Languages:** **TypeScript** (`scip-typescript` + `ts-morph`) and **Rust** (in-process tree-sitter + rust-analyzer) — both with type-checked, blast-radius-gated edits. **Python** rides an in-process tree-sitter fallback: full retrieval + skeletal outline + structural edits, but *ungated* (`gated: false`) until its LSP/indexer lands. The core (`ci-*` crates) is language-blind; a new language is a new provider implementing the same `LanguageProvider` trait.
- 16-crate Rust workspace, ~50 unit tests plus real-tool integration tests. See [docs/](docs/) for architecture, roadmap, and benchmarks.

## License

MIT — see [LICENSE](LICENSE).
