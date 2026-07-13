# Marksman

[![CI](https://github.com/d-weg/Marksman/actions/workflows/ci.yml/badge.svg)](https://github.com/d-weg/Marksman/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Precise code retrieval and type-checked edits for coding agents — over MCP.**

Marksman is a local-first [Model Context Protocol](https://modelcontextprotocol.io) server that gives an AI coding agent two things grep-and-guess can't:

- **Find** the exact code for a task — compiler-accurate symbols + an import graph, fused with semantic and keyword search, returned as a **line-ranged manifest** (not a pile of whole files to read).
- **Change** it safely — structured edits (rename / move / replace) applied atomically and **type-checked over the blast radius before they land**. A cross-file rename is *one* call, not N hand-edits, and nothing commits if it would break the build.

Written in Rust: a language-blind core plus per-language providers, in two support tiers:

- **First-class (type-checked, gated edits):** **TypeScript** and **Rust** — every edit is verified by the language's own compiler before it lands.
- **Experimental (gated, WIP):** **Java, PHP, Swift** — type-checked edits via the language's real compiler (`javac` / PHPStan / `swift build`), with cross-file rename through its LSP (jdtls / phpactor / sourcekit-lsp). Landed but **not yet benchmark-validated** — see the per-language pages under [docs/languages/](docs/languages/).
- **Best-effort (generic provider):** **Python, JavaScript, Go, Ruby, C, C++** — full retrieval, outlines, and structural edits via an in-process tree-sitter provider. Edits are **syntax-gated** (a change that no longer parses is rejected — tree-sitter never refuses input, so new ERROR nodes are the signal) but *not* type-verified (every response says `gated: false`); renames are within-file but come with a **server-side repo-wide verification scan** plus ready-to-copy fixes for anything the rename couldn't reach. Useful, honest — not the first-class experience.

## Why

Agents burn tokens grepping for context and break builds with blind string edits. Marksman hands the agent the *right* line-ranges and lets it make *type-checked* structural changes in one shot.

On a 10-task agent benchmark (Claude Sonnet, end-to-end, objectively checked), an agent **with** Marksman cost **~45% less and finished ~42% faster** than without, 10/10 correct — repo-wide type rename: **3 turns vs the baseline's 22** — and the same suite measured **−60%** when the client registered the MCP tools upfront instead of deferring them behind a discovery turn (a repo-wide rename is then **2 turns, $0.03**). The suite spans a multilanguage session (Rust + TS renames, each gated by its own compiler), the ungated fallback tier (Python + Go), a barrel-heavy repo, and a **workspace monorepo** (a cross-package schema change: 5 turns vs baseline's 10 — the type-checker enumerates the affected sites in *other packages*). A read-path ablation isolates what each layer buys — the compiler gate carries most of the win; SCIP's semantic import graph is what keeps the gate sound on barrels and package boundaries. The tool descriptions are audited to contain zero benchmark-specific content. Details and honest caveats: [docs/benchmarks.md](docs/benchmarks.md).

## Capabilities (MCP tools)

The surface is **two tools** — one to read, one to write. (The earlier six read/edit tools were
consolidated: `inspect`'s modes subsume `retrieve_context`/`find_symbols`/`list_anchors`/`read_node`/`describe_architecture`.)

| tool | what it does |
|---|---|
| `inspect` | Read/locate code — one tool, `mode`-dispatched: **`search`** (find code by concept/task text — hybrid BM25 + Model2Vec embeddings + symbol match, RRF-fused and expanded along the import graph; `detailLevel` pointers\|outline\|full), **`symbol`** (exact/substring name → self-locating node-id handles), **`file`** (a file's anchors + its import/module lines), **`node`** (one anchor's full source, or a `:body`/`:param.N`/`:return`/`:doc` sub-node), **`map`** (per-directory file-kind / module overview). Handles/ids feed `apply_edits` directly. No API calls. |
| `apply_edits` | Structured edits (`rename`, `move_file`, `replace_text`, `replace_node`, `set_body`, `insert_member`, `insert_in_body`, `add_parameter`, `add_symbol`, …), applied atomically and **type-checked over the blast radius** — nothing lands if it introduces a new type error, including in files that import what changed. Symbols are addressed by name (ambiguity auto-resolves from the edit's own text where possible), so a named target needs no `inspect` first. For wide changes, a rejection lists **every** affected site with its source and a ready-to-copy fix — the type-checker is the site-finder. |

## How it works

- **Read:** `scip-typescript` (compiler-accurate symbols + cross-file references) merged with in-process **tree-sitter** (sub-symbol AST), embedded by a native **Model2Vec** (`potion-code-16M`) embedder — no GPU, no embedding server — then indexed (BM25 + flat vector store + import graph, persisted as protobuf) and retrieved with **Reciprocal Rank Fusion** + graph expansion.
- **Write:** a persistent, warmed gate engine applies edits behind an in-memory VFS, gated by a baseline-diff of type diagnostics over the **blast radius** (the changed files + their importers). TypeScript's engine tiers are **tsgo → ts-morph → tsls**: the TS7 native LSP gates ~138× faster warm with identical verdicts and is auto-picked when locally present (`CI_TSGO`, or `tsgo` on PATH — never a surprise download); the ts-morph sidecar and `typescript-language-server` are the fallbacks. The LSP path prefers **LSP 3.17 pull diagnostics** (request/response — a slow server can never be mistaken for a clean file), which is also how the Rust gate drives rust-analyzer.
- **Fast startup:** the SCIP index is cached and validated by a content-hash fingerprint of the source (plus pinned tool versions) — a warm start on an unchanged repo is **~0.1s instead of ~26s**; any doubt reindexes, never a stale load.
- **Reads stay true in-session:** symbol ranges are re-anchored against the current file content on every read, and after a committed edit the write engine **re-describes the changed files** (new symbols, new import edges) back into the read path — a function you just added is immediately visible to `inspect` (`file`/`symbol` modes), no reindex. The Rust scip graph gets the same treatment: fingerprinted at build, with drifted/edited files served fresh tree-sitter edges.

## Install

Three steps — **build, index, register** — and no environment variables to set. Everything
below the build is **per language and lazy**: a toolchain is fetched or reported only for the
languages your repo actually contains.

### Prerequisites
- **Rust** (stable), to build Marksman — <https://rustup.rs>. Nothing else is needed to start.
- The **embedding model** (~65 MB, `minishlab/potion-code-16M`) **downloads itself** from
  Hugging Face on your first `index` — no manual step. (Offline? See the note below.)
- *Only if your repo has TypeScript:* **Node 18+** (`npx` fetches `scip-typescript`/`ts-morph`
  on first use).
- *Only if your repo has Rust and you want type-checked edits:* **rust-analyzer**
  (`rustup component add rust-analyzer`) — reads work without it.

`marksman doctor <repo>` reports exactly what your repo needs, what's installed, and the command
to install anything missing. A missing toolchain disables just that language, actionably — never
a silent half-degrade.

> **Don't want to install language toolchains?** [Container mode](#container-mode-optional)
> runs a language's whole toolchain (gate, rename engine — and for TypeScript, the indexer
> too) from a per-language OCI image at pinned versions: one container runtime instead of N
> toolchains, and the verdict can't drift with whatever's on the host.

### 1. Build
```bash
git clone https://github.com/d-weg/Marksman.git
cd Marksman
cargo build --release   # → target/release/marksman (CLI) and marksman-mcp (MCP server)
```

### 2. Index your repo
```bash
target/release/marksman index /path/to/your/repo   # writes .marksman/; fetches the model on first run
# optional sanity check:
target/release/marksman retrieve /path/to/your/repo "where is the rate limiter"
```

### 3. Register the MCP server
No environment variables needed — the model path, npm cache, and repo root all default.

**Claude Code:**
```bash
claude mcp add marksman -- /absolute/path/to/Marksman/target/release/marksman-mcp
```

**Any MCP client** (generic form):
```json
{
  "mcpServers": {
    "marksman": {
      "command": "/absolute/path/to/Marksman/target/release/marksman-mcp"
    }
  }
}
```
The server indexes the repo it's launched in (its working directory); or pass `--root /path/to/repo`
or set `MARKSMAN_ROOT`. Run `marksman index` (step 2) once before first use.

<details>
<summary>Offline / air-gapped, or a custom model location</summary>

The model auto-fetches over the network. To place it by hand, drop it at the default path and
Marksman finds it with no config:
```bash
mkdir -p ~/.marksman/models/potion-code-16M
curl -fL --output-dir ~/.marksman/models/potion-code-16M \
  -O https://huggingface.co/minishlab/potion-code-16M/resolve/main/model.safetensors \
  -O https://huggingface.co/minishlab/potion-code-16M/resolve/main/tokenizer.json \
  -O https://huggingface.co/minishlab/potion-code-16M/resolve/main/config.json
```
To keep it elsewhere, point `CI_MODEL_DIR` at that directory (in the shell for `index`, and in
the MCP server's `env` block). `CI_NO_MODEL_FETCH=1` disables the auto-download.
</details>

## Container mode (optional)

Instead of installing each language's toolchain, run it from a per-language OCI image —
**one container runtime instead of N toolchains, at pinned versions** (a host toolchain
upgrade can never silently change a verdict). All five gated languages are covered:
`marksman-ts` also runs the **indexer** (scip-typescript) in-container, so TypeScript needs
no host Node at all.

**Walkthrough: [docs/container-guide.md](docs/container-guide.md).** The helper does the
work — it detects your runtime, builds only the images you need, and checks the version pins
are in sync:

```bash
scripts/marksman-images.sh check          # runtime? images? pins in sync?
scripts/marksman-images.sh build ts rust  # build just what your repos need
CI_SANDBOX=oci marksman-mcp --root /path/to/repo   # opt in (per run, or in the MCP env block)
```

(The manual equivalent — `docker build -f docker/marksman-ts.Dockerfile -t marksman-ts
docker/` per language — still works; the helper just wraps it with runtime detection and the
pin check.)

How it behaves — see [docker/README.md](docker/README.md) for details:
- **Opt-in and loud.** Without `CI_SANDBOX=oci`, nothing changes (the host path is
  byte-identical). With it, a missing runtime warns and stays on the host at startup; a
  missing *image* errors loudly at the first operation — never a silent fallback to a
  different toolchain mid-session.
- **Runtime-generic.** The first of `container` (Apple), `docker`, `podman`, `nerdctl` found
  on PATH is used; `CI_SANDBOX_RUNTIME` picks one explicitly.
- One warm container per language, started lazily, reused across edits, removed on exit. The
  repo and the system temp dir are bind-mounted at their host paths, so nothing about your
  files changes.

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
| `CI_MODEL_DIR` | Model2Vec model directory | `~/.marksman/models/potion-code-16M` (auto-downloaded on first use) |
| `CI_NO_MODEL_FETCH` | set to disable the first-use model download (offline/air-gapped) | unset (fetch enabled) |
| `CI_NPM_CACHE` | npm cache dir for `npx` (scip / tsserver) | system temp |
| `CI_TSMORPH_DIR` | where the ts-morph sidecar is installed | `<tmp>/ci-tsmorph` |
| `CI_EDIT_ENGINE` | force a TS write-engine tier: `tsgo` · `tsmorph` · `lsp` (tsls, or whatever `CI_TS_LSP_SERVER` names) | auto: tsgo if local, else ts-morph, else tsls |
| `CI_TSGO` | path to a `tsgo` binary (TS7 native) — enables the fastest gate tier and the `CI_TS_MODE=lsp` sweep without npx | unset (PATH is probed) |
| `CI_TS_LSP_SERVER` | full command line for an alternative TS LSP server on the `lsp` tier (whitespace-split) | unset |
| `CI_PROVIDER` | `sidecar` runs the language provider as a separate process over a protobuf wire (`marksman-provider-<lang>`); unset = in-process | in-process |
| `CI_SANDBOX` | `oci` runs each language's toolchain in its `marksman-<lang>` container ([Container mode](#container-mode-optional)); unset = host toolchains | host |
| `CI_SANDBOX_RUNTIME` | the OCI runtime CLI to drive (name or absolute path) | first of `container`/`docker`/`podman`/`nerdctl` on PATH |
| `CI_GATE_TIMEOUT_SECS` | wall-clock ceiling for a gate verdict tool; a timeout REFUSES the edit (never passes, never downgrades the gate) | `600` |
| `CI_TS_MODE` | benchmark-reproduction knob only (read-path ablation arms + the `lsp` sweep producer; docs/benchmarks.md §2/§6) — not a supported configuration | `full` |
| `CI_SCIP_<LANG>` | overrides the `scip.<lang>` config setting (`1`=on, `0`=off). **Rust defaults ON**: the compiler-accurate `use` graph is generated on first open / refreshed at `index` when stale (≈ a `cargo check`), content-fingerprinted, with drifted files served fresh tree-sitter edges; `CI_SCIP_RUST=0` opts out | rust: on · others: the `scip.<lang>` config value |
| `MARKSMAN_ROOT` | repo root for the MCP server (legacy `CODEINDEX_ROOT` still honored) | current directory |

An optional `marksman.config.json` in the repo root (the legacy `codeindex.config.json` name is still read) overrides retrieval / index settings (top-N, RRF k, weights, …). For example, `{ "scip": { "rust": false } }` turns off the default `rust-analyzer scip` use-graph and serves the `mod`-only tree-sitter graph instead (`scip` is a per-language map; `CI_SCIP_RUST` overrides it per-run).

## Status

- **Languages:** **TypeScript** (`scip-typescript` + `ts-morph`) and **Rust** (in-process tree-sitter + rust-analyzer) — both with type-checked, blast-radius-gated edits. **Java** (`javac` + jdtls), **PHP** (PHPStan + phpactor), and **Swift** (`swift build` + sourcekit-lsp) are **experimental gated providers (WIP)** — type-checked edits have landed, benchmark validation is pending; per-language status, toolchains, and preliminary numbers are under [docs/languages/](docs/languages/). **Python, JavaScript, Go, Ruby, C, and C++** ride the generic in-process tree-sitter provider: full retrieval + skeletal outline + structural edits, but *ungated* (`gated: false`) until a language's LSP/indexer lands. JavaScript deliberately does **not** route through the TS toolchain today: scip-typescript/ts-morph only see JS when a tsconfig opts in via `allowJs`, and the gate is only as strong as `checkJs` — that path would claim "type-checked clean" on barely-checked code; gated JS is a roadmap item. **New languages roll out in two measured steps** ([benchmarks §3](docs/benchmarks.md)): first tree-sitter reads + the language's real compiler as the edit gate (benchmarked at −36% vs baseline with zero startup dependencies — most of the win), then a SCIP indexer as the maturity step for the cases where it's load-bearing: monorepos and cross-package blast radius. TypeScript and Rust already have their SCIP layer and keep it — it is not removable, it's what makes their gate sound without hand-verification. Adding a fallback language is a grammar dependency plus a few table rows; upgrading one to gated is swapping in a `GateEngine`. The core (`ci-*` crates) is language-blind.
- 16-crate Rust workspace, ~50 unit tests plus real-tool integration tests. See [docs/](docs/) for architecture, roadmap, and benchmarks.

## Contributing

PRs welcome — [CONTRIBUTING.md](CONTRIBUTING.md) covers the build, the two test tiers (fast
unit tests in CI; real-tool e2e suites to run locally before provider/gate changes), the
benchmark rules, and the project's non-negotiables (never serve stale reads, never silently
degrade a gate).

## License

MIT — see [LICENSE](LICENSE).
