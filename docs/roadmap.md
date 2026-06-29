# CodeGraph — roadmap

Directions, not commitments.

## North star: a pure-Rust core; languages as modular, on-demand providers

**The core needs nothing but Rust to run.** Indexing, the native Model2Vec embedder, BM25,
RRF, retrieval, the VFS, and the edit-gate *orchestration* are all pure Rust today. Any
external or runtime dependency (Node, an indexer, a language server) belongs to a **language
provider**, never to the core. This is the load-bearing invariant.

**Providers are modular and loaded on demand.** Indexing a repo should *detect which languages
are present* (by extension) and activate only those providers. A Rust-only repo must never
invoke Node; a repo with no TypeScript must never fetch `scip-typescript` or `ts-morph`. Each
provider declares its own toolchain and **fetches it lazily, the first time a file of that
language is indexed** — you download only what the repo actually needs.

**Where we are vs. that goal.** `build_index` is already generic over the `LanguageProvider`
trait, and Node is confined to `lang-ts`. But the CLI/MCP entry points still **hardcode
`TsProvider`** (`ci-cli/src/main.rs`, `ci-mcp/src/main.rs`), so the TS indexer (Node) runs even
on a non-TS repo. Closing that is the next structural step.

### Work items
1. **Provider registry + extension dispatch.** A map `extension → provider`; `index` derives
   the active provider set from the files actually present and routes each file through its
   owning provider (so multi-language repos work). Node is touched *only* if `.ts/.tsx` exist.
2. **Lazy, on-demand tooling.** Each provider fetches its toolchain on first use and caches it
   under a per-provider dir (TS: `scip-typescript` / `ts-morph` via npm; Rust: a pinned
   `rust-analyzer`/scip binary or the `ra_ap_*` crates). No global installs; nothing fetched
   for languages the repo doesn't contain.
3. **Provider manifest** so the set is discoverable and configurable — enable/disable a
   language, pin a tool version, point at a vendored binary for offline/air-gapped use.

## Languages

### 1. Rust — next, and the reason it's first
Rust is the highest-leverage second language because **Rust's own tooling is Rust**, so a Rust
provider gets far closer to the "single binary, no foreign runtime" ideal than TypeScript ever
can (TS forces us out to Node):

- **Structure / sub-symbol AST:** `tree-sitter-rust` — already a workspace dependency,
  in-process, zero external deps.
- **Compiler-accurate symbols + references (read):** `rust-analyzer scip` (a single static
  Rust binary — bundle or download once, no Node/npm), or the `ra_ap_*` crates linked
  in-process for a true zero-external-process build.
- **Type-checked edits (write/gate):** rust-analyzer's rename / references / diagnostics,
  slotting into the existing `GateEngine` trait exactly as the ts-morph engine does today.
- **Dogfooding:** once CodeGraph can index and edit Rust, we use *it* to build the remaining
  providers — the tool accelerates its own development (it's a Rust codebase).

### Then, via the generic LSP `GateEngine` fallback (already built)
- **Python** — pyright + a SCIP-python indexer.
- **Go** — gopls + scip-go.
- **Java / C# / …** — each a new crate implementing `LanguageProvider`, reusing the
  language-blind core, retrieval, VFS, and blast-radius gate **unchanged**.

The seams already exist: `LanguageProvider` (structure + import graph + edits), `Granularity`
(Symbol vs Ast), and `GateEngine` (ts-morph or LSP). A new language is a new crate, not a core
change.

## Benchmarks (planned)

### Three-way: read/edit backend — speed AND precision
Same operations across three implementations on the same real repos:

| Variant | Read | Edit granularity | Notes |
|---|---|---|---|
| **Rust + SCIP only** | SCIP (`scip-typescript`) | symbol-level (`replace_node`) | compiler-grade refs, no sub-symbol edits |
| **Rust + SCIP + tree-sitter** (current) | SCIP + in-process tree-sitter | **sub-symbol** | `Granularity::Ast`, no external dep for the AST |
| **Node** (original) | ts-morph | sub-symbol | the oracle |

Measure: indexing speed (cold/warm, small vs monorepo); retrieval precision (manifest overlap +
ranking vs a labeled set); edit precision/coverage per op class; end-to-end edit latency. The
agent A/B benchmark (with vs without, vs the TS tool) lives in [benchmarks.md](benchmarks.md).

## Other directions
- **Fine verbs over the AST tree** — `set_body` / `set_return_type` / `add_parameter` now have
  targets (`#sym:body` / `:return` / `:param.N`) via the SCIP+tree-sitter merge; remaining work
  is mapping them in `action_to_op` + non-ASCII (byte-based) column handling.
- **Incremental index refresh** after a commit (reindex only changed files; `scip-typescript`
  is largely whole-project — measure latency, consider a faster path).
- **Persisted package roles** (deps-based `infer_role` at index time) for sharper query
  weighting.
- **TS-tool ranking parity** — port the retrieval fixes (path-role, symbol-match, the
  `name`-in-`rename` word-boundary fix) into the original Node `codeindex` so the A/B isolates
  read-vs-write mechanics.
