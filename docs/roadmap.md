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

## Backlog (actionable, in dependency order)

- [x] **Rust provider — read path (done).** `lang-rust` crate: `structure()` (fns/structs/
      impls/methods + `#sym:body`/`:param.N`/`:return`), `import_graph()` (`mod` resolution),
      `granularity()→Ast`, all in-process `tree-sitter-rust` (no Node). Indexes + retrieves Rust
      (incl. CodeGraph itself) via CLI and MCP.
      - [ ] **Rust write path** — edits behind a `GateEngine` over rust-analyzer (rename/refs/
            diagnostics). `apply_edits` currently returns a read-only refusal for Rust.
      - [ ] **Better Rust graph** — `use`-resolution (not just `mod` edges) + optional
            `rust-analyzer scip` for compiler-accurate references.
- [x] **Provider selection (done, v0).** `build_provider`/`select_provider` keyed on manifests
      (Cargo.toml vs package.json; `CI_LANG` override) in `ci-cli` + `ci-mcp` — **Node only for a
      TS repo now.**
      - [ ] **Full registry** — multi-language repos (per-file dispatch), lazy per-language
            tooling fetch, a provider manifest (enable/disable, pin versions).
- [ ] **Skeletal context** — `detail_level` (`full`/`outline`/`signatures`) on
      `retrieve_context`; reuse `lang-ts/src/ast.rs` body location to elide `statement_block`s;
      secondary import-graph files default to `outline`; add a `read_node` drill-down tool.
- [ ] **Surgical sub-node edits** — map `insert_in_body`/`replace_in_body`/`delete_in_body`/
      param/return/comment verbs in `ci-edit::action_to_op` over the existing
      `#sym:body`/`:param.N`/`:return` anchors; keep them gated.
- [ ] **Config providers (JSON/YAML/TOML)** — tree-sitter providers for surgical key edits
      (package.json, compose, *.toml); no gate needed. Rides on the provider registry.
- [ ] **Tree-sitter fallback edit provider** — ungated structural edits for languages without
      SCIP/LSP (Python/Go/…); result flags `gated: false`. Rides on the registry; upgraded
      per-language to the gated path over time.

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

## Capabilities (planned)

### Skeletal context — signature-level retrieval (`detail_level`)
An agent rarely needs a 200-line function body just to know how to *call* it. Add a
`detail_level` to `retrieve_context` — **`full` | `outline` | `signatures`** — and use the
in-process tree-sitter we already have (`lang-ts/src/ast.rs` already locates each symbol's
`#sym:body`) to **elide bodies**: replace a `statement_block` with `{ /* … elided */ }`, keeping
the signature, params, and return type. A 200-line file collapses to ~15 lines of pure signal,
with compiler-accurate types intact.
- **Secondary files default to `outline`.** Files pulled in via the import graph are context,
  not the target — you need their *signatures* to call them, not their bodies. The primary
  matches can stay `full`.
- **Node drill-down.** When the elided body *is* what's needed, a tool to fetch the precise full
  text + metadata (kind, signature, exact range, refs) of a single anchor — reusing the existing
  `#sym:body` / `:param.N` / `:return` anchors. Skeleton by default, expand on demand.
- Pure tree-sitter, no SCIP/compiler needed for the fold → cheap, and works for any language
  with a tree-sitter grammar.

### Surgical sub-node edits (deeper `apply_edits` verbs)
`replace_node` is sometimes too blunt — re-drafting a whole function to tweak one statement.
Expand the edit suite to map directly onto AST anchors (the `#sym:body` / `:param.N` / `:return`
targets already exist from the SCIP+tree-sitter merge):
- **Body-level:** `insert_in_body` / `replace_in_body` / `delete_in_body` — inject or remove a
  single statement inside a block/loop/conditional without reconstructing the parent.
- **Targeted modifiers:** add/remove a **parameter**, set a **return type**, edit a call's
  **arguments** — without re-emitting surrounding syntax.
- **Comments / docstrings:** `edit_leading_comment` / docstring control — touch documentation
  without touching executable code.
- Remaining work: mapping these verbs in `action_to_op` + statement-level targeting within a
  body + byte-vs-char column handling. **CodeGraph's edge over a pure-AST editor: these stay
  type-checked** (the blast-radius gate) — surgical *and* safe.

### Config files as first-class citizens (JSON / YAML / TOML)
Agents constantly need to edit `package.json`, `docker-compose.yml`, `*.toml` alongside code
(e.g. add a dependency when adding an import). Add tree-sitter providers for json/yaml/toml so a
single **key** can be modified surgically — no reformatting, no clobbered comments. These need no
compiler gate (config has no type-check), so they're pure tree-sitter and slot straight into the
modular-provider model. High synergy: update `package.json` deps **in the same atomic batch** as
the TS import that needs them.

### Breadth vs. depth — a pure-tree-sitter fallback edit provider
The real axis: **breadth** (tree-sitter — 11+ languages, no type safety) vs **depth**
(SCIP/LSP — type-checked, few languages). CodeGraph can do *both* through the existing
`Granularity` + `GateEngine` seams:
- A **tree-sitter structural-edit provider with no blast-radius gate** is the *fallback* for any
  language we don't yet have SCIP/LSP for (Python, Go, Ruby, …) — structural edits work
  immediately, best-effort.
- Per language, upgrade to the **gated** path as the SCIP/LSP integration lands (Rust first).
- So a multi-language repo is *useful on day one*, and the type-checked guarantee is layered in
  where the toolchain exists. **Honest tradeoff:** tree-sitter edits aren't type-checked — the
  result must say so (e.g. `gated: false`) so the agent knows it's structural, not verified.
