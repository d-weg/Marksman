# Marksman — architecture

A from-scratch Rust rewrite of the TS tool `codeindex`: local, zero-API code retrieval +
gated structural edits, **language-agnostic** via a provider seam.

## Core principle

> **The core is pure Rust and needs nothing external to run. Anything outside Rust —
> and anything language-specific — lives in the language provider, never in the core.**

- **Core (pure Rust, in-process):** `ci-core`, `ci-walk`, `ci-embed`, `ci-index`,
  `ci-retrieve`, `ci-build`, `ci-vfs`, `ci-lsp` (generic LSP *transport*), `ci-arch`,
  `ci-cli`. No subprocess, no Node, no per-language knowledge.
- **Provider (`lang-ts`) owns all external/language-specific deps:** `scip-typescript`
  (read artifact), the gate engines (`tsgo` when locally present, else the ts-morph
  sidecar, else `typescript-language-server`) — external tools via `npx`, no global
  install — and the in-process `tree-sitter-typescript` grammar. Adding a language = a new
  provider crate, not a change to the core.

`ci-lsp` is a language-agnostic LSP client: the provider passes it the server `Command`;
`ci-lsp` itself knows nothing about TypeScript.

## How it works

```
Agent / CLI / MCP  (pure Rust)
        │
   ci-core  (BM25 · Model2Vec · RRF · weighting · retrieval)   ← language-blind
        │  LanguageProvider:  granularity() · structure()->Node tree · import_graph() · apply_edits()
        ▼
   a language provider  =  ReadIndex  ×  WriteEngine
        │                     │              │
        │              the ARTIFACT     the live CHECKER
        │              (loaded, O(1))   (warm process)
        │                     │              │
        │    TS:  scip-typescript index  ×  tsgo LSP → ts-morph → tsls
        │    Rust: tree-sitter (live) + ra scip use-graph  ×  rust-analyzer
        │    new:  tree-sitter or the ci-lsp-index sweep  ×  the language's LSP
        ▼
     READ   ── the artifact + tree-sitter deepening ── what the agent PLANS against
     WRITE  ── VFS overlay (ci-vfs) + baseline-diff gate over the blast radius
```

### The two halves: `ReadIndex` × `WriteEngine`

A provider is two independently swappable parts, and they are DIFFERENT KINDS of thing —
the naming is deliberate:

- **`ReadIndex`** (`ci-core`) — the read half is an **artifact or a live parser, never a
  running checker**: a loaded SCIP index, or tree-sitter over current disk. It answers the
  *planning* phase — many speculative queries over unpredictable symbols (who uses this, how
  central is it, what breaks) where most answers are discarded. That access pattern needs
  O(1) lookups from something already loaded; measured on microsoft/TypeScript, rebuilding
  the same answers with live per-symbol LSP queries costs 38× the indexer's one compiler
  pass (`docs/benchmarks.md`). **Planning needs the artifact.**
- **`GateEngine`** (`ci-edit`) — the write half IS a live process: by edit time the changed
  symbols are known, the queries are few and targeted, and the answer must reflect the
  post-edit state, not a snapshot. TypeScript's engine tiers: **tsgo** (TS7 native LSP —
  ~138× faster warm gate, identical verdicts; auto-picked only when it costs no network:
  `CI_TSGO` or `tsgo` on PATH) → **ts-morph sidecar** → **tsls**. `CI_EDIT_ENGINE=
  tsgo|tsmorph|lsp` forces a tier; `CI_TS_LSP_SERVER` swaps the LSP-tier server command.

`ci_edit::Composed<R: ReadIndex>` assembles the two halves into a `LanguageProvider` —
and it is the ONLY assembly: `lang-ts`, `lang-rust`, the `TsTreeGated` ablation, and
`lang-template` all go through it; no provider wires the channels by hand. The halves
talk over **exactly three channels**, and the wiring policy is *derived* from properties
the reader advertises — not hand-wired per language:

| channel | direction | policy source |
|---|---|---|
| **radius** — the reverse-import set the gate checks | read → engine | `semantic_edges()`: compiler-accurate graphs (SCIP) flatten barrels, one hop is sound; syntactic graphs must expand **transitively** or a barrel hides its consumers (bench T9) |
| **freshness** — post-commit read overrides | engine → read | `live()` / `live_graph()`: live readers re-parse disk, nothing to do; artifact-backed structure takes `file_summaries` overrides, an artifact-backed graph takes edge overrides. A **hybrid** reader (Rust: live tree-sitter structure over an artifact scip graph) splits the policy per half — that is why the two bits exist |
| **anchors** — edit ops resolve against the structure the agent saw | read → engine | always: the ids `list_anchors` advertised must be the ids `apply_edits` accepts |

Where a language's reality doesn't fit the generic recipe, the provider registers a
**hook at assembly time** — never a fork of the glue:

- **`Prewarmer`** — rust-analyzer must be warmed by pulling one real file's diagnostics
  against the raw LSP client (the default recipe would route through `RustEngine` and
  spawn a `cargo check` at startup); the TS engines warm on a sample project file.
- **`LiveSummarizer`** — the freshness fallback when the engine can't re-describe files
  (LSP-tier TS engines return no `file_summaries`; TS registers a `lang_fallback`
  tree-sitter re-parse, Rust a `mod`+`use` edge summarizer for its artifact graph).
- **`FreshDeepener`** — TS re-runs the tree-sitter deepen over structure served from a
  fresh summary, so post-commit reads keep their `:body`/`:params`/`:return` anchors.

Language-specific op *synthesis* stays in the provider, applied before delegation (Rust
expands `create_file` with the `pub mod` declaration an orphan `.rs` file needs) —
`Composed` only ever sees a final batch.

**Artifact producers are swappable too.** The same `index.scip` consumer accepts:
`scip-typescript` (the default for TS — earns its cold-index cost at scale), the
**`ci-lsp-index` sweep** (documentSymbol + references over any LSP → a genuine SCIP
protobuf; `CI_TS_MODE=lsp` arm — full parity on fixtures/bench, 38× slower at 380k-line
scale, so it is the producer for languages that have an LSP but *no* scip indexer, not a
scip replacement), or tree-sitter directly (live, syntactic). The conformance suite pins
producers to the same expectations (`conformance_ts_scip` / `conformance_ts_lsp_sweep`).

### How a provider crate is divided

One capability, one module. The **core crates own every generic capability** — a
language crate holds only what is genuinely per-language, and its files split cleanly
by which half they feed:

```
        ┌─────────────────────── a language provider crate ───────────────────────┐
        │                                                                          │
        │   lib.rs — ASSEMBLY ONLY: build the read half × the write half,          │
        │            hand both to ci_edit::Composed, register hooks                │
        │                                                                          │
        │   READ HALF (feeds ReadIndex)          WRITE HALF (feeds GateEngine)     │
        │   ─────────────────────────────        ────────────────────────────────  │
        │   lang-rust:                           lang-rust:                        │
        │     structure.rs  items + sub-nodes      gate.rs     cargo-check gate,   │
        │     graph.rs      mod/use resolution,                 ra rename,         │
        │                   scip cache + drift                  deleted-ref gapfill│
        │                   seeding                movefix.rs  module-move rewriter│
        │                                                                          │
        │   lang-ts:                             lang-ts:                          │
        │     ast.rs        SCIP+tree-sitter        engine.rs   tsgo→ts-morph→tsls │
        │                   merge, re-anchor                    ladder + npx cache │
        │     fingerprint.rs cache invalidation     tsmorph.rs  sidecar client     │
        │     outline.rs    body elision            sidecar.cjs ts-morph ops       │
        │                                                                          │
        │   ablation.rs (ts): TsTreeGated = Composed<FallbackProvider> — the       │
        │   tree-sitter-read ablation is just a different read half, same engine   │
        └──────────────────────────────────────────────────────────────────────────┘

Generic capabilities live in the core, one module each — a provider DELEGATES, never
reimplements (enforced by the executable §7 audit in ci-conformance):
  ci-core:        fingerprint (cache drift) · paths (rel_path) · driver (the traits)
  ci-edit:        actions (op vocabulary) · apply (op handlers) · composed (the glue) ·
                  lib (GateEngine + the commit_edits spine) · spawn_prewarm
  ci-treesitter:  outline (body elision) · sub-node helpers
```

### Read = SCIP **+** tree-sitter (merged)
- **SCIP** gives compiler-grade semantics: symbol identity, cross-file references → the
  import graph, and precise `enclosing_range` per symbol. External, but precise.
- **tree-sitter** (in-process, no external tool) subdivides each SCIP-anchored symbol into
  **local** sub-nodes (`parameter` / `returnType` / `body`). Used ONLY for local syntax
  inside a range SCIP already pinned — never for semantics — so its precision limits don't
  bite. This makes `structure()` a **deep AST tree** → `Granularity::Ast`.
- Net: SCIP's "no AST → no sub-symbol edits" weakness is solved by tree-sitter; tree-sitter's
  "no semantics" weakness is covered by SCIP. Each fixes the other.

### Read freshness (Composed-owned — the index never lies in-session)

One generic channel, implemented once in `Composed`: after a committed `apply_edits`,
the engine re-describes the changed files (`file_summaries`), or the provider's
registered `LiveSummarizer` approximates from current disk when it can't; the per-file
overrides then shadow the artifact (replace / blank-on-empty / drop-on-delete) until the
next reindex. What differs per provider is only the reader's policy bits and which
summarizer it registers:

- **TS** (`live()=false` — everything artifact-backed): the ts-morph sidecar
  re-describes natively (`fileInfo`: symbols + resolved imports); the LSP engines
  (tsgo/tsls) can't, so the `lang_fallback` tree-sitter summarizer covers them. Fresh
  structure passes through the `FreshDeepener` so sub-node anchors survive the overlay;
  scip fidelity returns at the next reindex. Startup stays fingerprint-cached.
- **Rust** (hybrid: `live()=true`, `live_graph()=false` when the scip graph serves):
  structure re-parses disk — nothing to overlay; only the artifact use-graph takes edge
  overrides. Load-time staleness never reaches the glue: the read half bakes
  fingerprint-drifted files' edges into its base graph itself (tree-sitter `mod` +
  resolved `use`), and a cache with **no** fingerprint is refused (mod-graph fallback),
  never served at unknown age.

### Retrieval flow (read side)

```
inspect(search: task text)                       inspect(symbol/file/node/map)
  │                                                │
  ├─ BM25 (sparse, ci-index)      ┐                └─ direct index/structure lookups
  ├─ Model2Vec (dense, ci-embed)  ├─ RRF fusion → seeds
  └─ symbol-name match            ┘
  ▼
graph expansion — import edges, N hops, both directions (ci-retrieve)
  ▼
package weighting → Manifest: weighted files + symbols
  = what the agent PLANS against (the artifact answers; no checker runs)
```

### Write = VFS transaction + LSP gate

```
apply_edits(batch)
  │  action strings → EditOp (ci-edit/actions) · provider synthesizes language ops
  │  anchors resolve against the structure the agent saw (read half)
  ▼
VFS overlay (ci-vfs) — structural ops bottom-up per file; rename/move as engine
  │                    WorkspaceEdits; disk untouched
  ▼
blast radius — reverse imports from the read half's graph
  │             one-hop when semantic_edges(), else TRANSITIVE (barrels can't hide)
  ▼
GATE — engine diagnostics over the radius, baseline-diffed (only NEW errors count)
  ├─ reject → anchored, self-sufficient reply (offending source + ready-to-copy fix);
  │           rollback — disk byte-identical
  └─ clean  → atomic commit → freshness channel updates the read half
              → incremental reindex (changed files only)
```

- Edits stage into an in-memory **VFS** overlay (`ci-vfs`); disk untouched until commit.
- The **gate**: push `didChange` with the overlay to the language server (`ci-lsp`),
  collect in-memory diagnostics, **baseline-diff** (fail only on NEWLY introduced errors),
  then commit-or-roll-back atomically. `ci-lsp` prefers **LSP 3.17 pull diagnostics**
  (`textDocument/diagnostic`, gated on a quiescent `experimental/serverStatus` for
  rust-analyzer) — request/response, so a slow server can never read as "clean"; the
  publish+silence-settle path remains only for servers without pull (tsls).
- **rename** via LSP (all references); **replace_node / insert / replace_text** via ranges;
  with `Granularity::Ast`, sub-symbol edits (`#sym:body`, `#sym:return`, `#sym:param.N`)
  work through the same machinery.
- Rejections are **anchored**: each error names the op that introduced it (scoped repair).
- **reindex-on-commit**: `update_index` re-embeds only the changed files.

## Crate status

| Crate | Role | Status |
|---|---|---|
| `ci-core` | types, `LanguageProvider`, `Node` tree + `Granularity`, config, weighting | ✅ |
| `ci-walk` | discovery (gitignore), workspace/package detection | ✅ |
| `ci-embed` | native Model2Vec (tokenizers + safetensors), parity 1.0 vs Python | ✅ |
| `ci-index` | BM25 + vector store + import graph + persistence | ✅ |
| `ci-retrieve` | RRF + graph expansion + package weighting → Manifest | ✅ |
| `ci-scip` | SCIP `index.scip` → Node tree + import graph | ✅ |
| `ci-build` | build pipeline + incremental `update_index` | ✅ |
| `ci-vfs` | in-memory overlay transaction | ✅ |
| `ci-lsp` | **generic** LSP transport (provider supplies the command) | ✅ |
| `ci-lsp-index` | SCIP emitter over any LSP (documentSymbol + references sweep) | ✅ |
| `ci-edit` | gated atomic edits + anchored repair, split by capability: `actions` (op vocabulary) · `apply` (op handlers) · `composed` (ReadIndex × GateEngine glue) · the `commit_edits` spine | ✅ |
| `lang-ts` | TS provider = `Composed<TsRead>`: SCIP + tree-sitter read half; gate tiers tsgo → ts-morph → tsls | ✅ |
| `lang-rust` | Rust provider = `Composed<RustRead>`: live tree-sitter structure + `rust-analyzer scip` use-graph (fingerprinted, drift-seeded) read half; cargo-check gated write | ✅ |
| `lang-fallback` | GENERIC tree-sitter provider (Python, Go, Java, Ruby, C, C++): read path + ungated edits; the `ReadIndex` reference impl | ✅ |
| `lang-template` | copyable Step-1 skeleton: `Composed` over tree-sitter reads + your checker | ✅ |
| `ci-cli` | `index` / `retrieve` binaries | ✅ |
| `ci-arch` | zero-API architecture map (detects module templates) | ✅ |
| `ci-mcp` | Rust MCP server (stdio): the two-tool facade — `apply_edits` + `inspect` (search/symbol/file/node/map) — pinned by a surface test | ✅ |

**~150 unit tests + real-tool e2e** (scip-typescript indexing, LSP gate, edit gate, the
SCIP+tree-sitter deepen, the conformance battery incl. the executable §7
no-reimplementation audit).

## Done vs pending

- **P0** scaffold + core types/trait ✅
- **P1** SCIP-read index + retrieval — real-repo parity vs Node (13/18 on the RRF task) ✅
- **P2** SCIP+LSP+VFS gated edits + anchored repair + incremental reindex ✅
- **SCIP + tree-sitter merge** → `Granularity::Ast`, sub-symbol edits ✅
- **P3:** file ops (`move` via LSP `willRenameFiles`, `delete` via reverse-graph safety) ✅ ·
  `ci-arch` ✅ · `ci-mcp` (Rust MCP server, all 4 tools verified over stdio) ✅ ·
  benchmarks ✅ (`docs/benchmarks.md`)
- **Headline benchmark:** the SCIP+tree-sitter merge costs **+1.2% (~40ms)** at index time;
  Rust indexes ~4× faster than Node; retrieval ≈55% Jaccard overlap (different embedder + graph).

All planned phases (P0–P3 + the SCIP+tree-sitter merge) are complete.

## Verification notes
- Build needs a C compiler (for `tokenizers` + `tree-sitter` grammars) — present on macOS.
- `npx`-based tools run with a fresh `CI_NPM_CACHE` dir to dodge a corrupted default
  `~/.npm` cache (permission/rename errors).
- Model files resolve from `$CI_MODEL_DIR` (Model2Vec dir), default the sibling Node repo's
  `potion-code-16M`.
