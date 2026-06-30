# Marksman — roadmap

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

## Addressing model — intent over location (token-minimal editing)

The cost of an agent run is **turns × ~fixed-per-turn-context** (the harness re-sends everything
each turn; caching discounts but the metric/cost still scale with turns). So the lever is *fewer
turns* — and a big source of wasted turns is the agent **locating** things the index already knows
and **reading** context it doesn't need. Two principles fix this:

1. **The type-check gate licenses minimal context.** Because every edit is verified over the blast
   radius, the agent can edit *locally and blind* and trust the gate — it never needs to read
   call sites defensively. That's the real reason gated editing is cheap (not just atomicity).
2. **Address by handle, not by hand.** A node id (`file#Scope.name`, optionally `:body`/`:doc`/…)
   is unique AND self-locating. `apply_edits`/`read_node` resolve a reference cheapest-precision
   first (**done**): a node id is used as-is (no `path`/`file`, no retrieve); a bare name is
   resolved in a given file, else **across the index** (so the agent edits by name with no prior
   retrieve — the index supplies the file); a same-name collision returns the **candidate ids** to
   re-issue with (one cheap round-trip, never a full retrieve). Local variables (not indexed) stay
   position-addressed via the LSP. Verified: rename `reciprocalRankFusion`→`fuseRanks` by name with
   no path (2 files, gated); `read_node` by id with no file; ambiguous `freshProject` → 2 candidates.
   - [ ] Surface the qualified node id in `retrieve_context`'s matched-symbol lines so the handle
         propagates directly (today the agent reconstructs `file#name` or uses bare-name resolution).
   - [ ] `read just the sub-node`: encourage `read_node id=…:body`/`:doc` so a body edit loads only
         the body, not the whole symbol.
   - [x] **Resolve-by-query (`retrieve_and_edit`) — done.** A fourth, fuzziest addressing mode: an
         `apply_edits` action with `query` (free text) instead of `name`/`id`. The server resolves
         it — an exact symbol-name token in the query when unique, else the retrieval top hit —
         and applies the edit (gated) when unambiguous, else returns candidate ids. Fuses
         locate+edit into ONE call, closing the T3 discovery turn. Safe by construction: the gate +
         `replace_text` uniqueness reject a wrong/ill-fitting target rather than silently editing
         it. (Honest limit: a descriptive query rides retrieval ranking; a mismatched edit is
         rejected, not misapplied.)

## Backlog (actionable, in dependency order)

- [x] **Rust provider — read path (done).** `lang-rust` crate: `structure()` (fns/structs/
      impls/methods + `#sym:body`/`:param.N`/`:return`), `import_graph()` (`mod` resolution),
      `granularity()→Ast`, all in-process `tree-sitter-rust` (no Node). Indexes + retrieves Rust
      (incl. Marksman itself) via CLI and MCP.
      - [x] **Rust write path (done).** Full structural-edit coverage — **rename, replace_node,
            move_file** — type-checked via rust-analyzer, reusing the `GateEngine`/`LspClient`/
            `commit_edits` blast-radius gate (all three verified end-to-end). Needs
            `rustup component add rust-analyzer` (`CI_RUST_ANALYZER` overrides). Next: faster
            cold-start than the ~10s rename/move retry (a "rust-analyzer ready" signal).
      - [x] **Better Rust graph (done, opt-in).** `CI_RUST_SCIP=1` swaps the `mod`-only graph for a
            **compiler-accurate `use`/reference graph** from `rust-analyzer scip` — read by the
            existing `ci-scip` (no reader changes; validated). Latency-safe: `lang_rust::refresh_scip`
            generates `<root>/.codeindex-rs/rust.scip` at **index time** (a batch step, ≈ a
            `cargo check`); `import_graph()` only *reads* the cache and **falls back to the instant
            tree-sitter `mod` graph** when it's absent — SCIP never sits on the live path. Off by
            default (the analysis latency); flip on per repo. tree-sitter still owns structure +
            sub-nodes, so Rust now mirrors TS: tree-sitter for AST, SCIP for the semantic graph.
- [x] **Provider selection (done, v0).** `build_provider`/`select_provider` keyed on manifests
      (Cargo.toml vs package.json; `CI_LANG` override) in `ci-cli` + `ci-mcp` — **Node only for a
      TS repo now.**
      - [ ] **Full registry** — multi-language repos (per-file dispatch), lazy per-language
            tooling fetch, a provider manifest (enable/disable, pin versions).
- [ ] **Dynamically-fetched modular providers (sidecar processes).** Today the provider glue
      crates (`langs/lang-ts`, `langs/lang-rust`) are compiled into the core binary, even though
      their *tooling* (scip-typescript, ts-morph, rust-analyzer) is already fetched on demand. To
      make a provider itself downloadable: ship each as its own small executable, fetch it on
      demand (release/registry), and have the core spawn it and talk over a **stdio JSON protocol**
      (serialize `LanguageProvider`: structure / import_graph / apply_edits). Core stays tiny +
      language-agnostic; providers can be third-party / any language. NOT dlopen — Rust has no
      stable ABI, so in-process plugins are unsafe/undistributable; separate processes (the model
      rust-analyzer / the ts-morph sidecar already use) are the correct seam.
      - [x] **Protocol + host + both sidecars + dispatch (done) — protobuf over JSON for speed.**
            `ci-proto` (prost messages + ci-core↔proto conversions + `[u32-len][bytes]` framing)
            defines the wire; `ProcessProvider` (host) spawns a provider binary and implements
            `LanguageProvider` over it; **`marksman-provider-rust`** (serves `RustProvider`) and
            **`marksman-provider-ts`** (runs scip-typescript, then serves `TsProvider`) are the
            sidecars — the gate (rust-analyzer / ts-morph) runs *inside* the sidecar, so
            `apply_edits` travels the wire too. `CI_PROVIDER=sidecar` routes the CLI `index` and the
            MCP server through a sidecar (binary resolved via `$CI_PROVIDER_<LANG>` or next to the
            exe; falls back to in-process if absent). Tested: read path + `apply_edits` over the wire
            (gate in-sidecar); dogfooded end-to-end (`index` + `list_anchors` over the wire).
            Protobuf is compact + cheap to decode at indexing volume — it speeds the provider RPC /
            `sec` axis, NOT the agent token count (that's turns).
      - **Default model: providers live in-repo under `crates/langs/`, built + shipped together.**
            Each is a crate with a `marksman-provider-<lang>` bin; one `cargo build` produces them
            all next to the core, and `CI_PROVIDER=sidecar` spawns the one the repo needs (resolved
            next to the exe). Adding a language is a new folder in `langs/` — no registry. You still
            get the runtime modularity (out-of-process, language-agnostic wire) without a download
            system. **This is the chosen model.**
      - [ ] *(optional, future)* **Downloadable providers** — only if you want a *slim core* that
            doesn't bundle every provider binary, or *third-party* providers published independently:
            fetch `marksman-provider-<lang>` on demand from a release/registry + cache + version-pin.
            Not needed for a repo you control; deferred until there's a reason.
- [x] **Skeletal context (done).** `detailLevel` (`pointers`/`outline`/`full`) on
      `retrieve_context` inlines the top files with fn/method bodies folded to `{ /* … */ }`
      (`ci-core::elide_bodies` + tree-sitter `outline` in each provider). TS + Rust.
      - [x] `read_node` drill-down tool (done) — full source + metadata of ONE anchor by `id`
            or `name`, incl. `:body`/`:param`/`:return` sub-nodes.
      - [x] **secondary import-graph files auto-default to `outline` (done)** even when the call
            asks for `full` on the primaries — they're labeled "(outline — imported context)" so
            the agent knows to `read_node` if it needs a body. Inline caps tightened (4×100) so a
            big retrieve isn't re-read every turn.
- [x] **Surgical sub-node edits (done, v0).** Edit part of a function without re-emitting it,
      still type-checked. `set_body` (new verb) replaces just the `:body` block; `replace_node`
      takes a `target` to narrow onto a sub-node anchor — `body`, `return`, or `param.N`; and
      **`replace_text`** swaps an exact substring inside a symbol (`oldText`→`newText`, unique
      within it) — the cheapest precise edit: no read, no body re-emit, and a not-found error now
      echoes the node's text so the agent self-corrects. Wired in `ci-edit::action_to_op` +
      the MCP `apply_edits` schema. All ride the blast-radius gate (verified on Rust + Python).
      The agent-benchmark T4 win (rust −27% vs ts +25%) comes from `replace_text`.
      - [ ] **Statement-level body edits** — `insert_in_body`/`delete_in_body` targeting ONE
            statement inside a block (replace-in-body is already covered by `replace_text` +
            `target:"body"`; insert/delete still need statement addressing).
      - [ ] **`add_parameter` / `set_return_type` where absent** — insert `-> T` / a new param
            when there's no existing anchor (needs the params-end insertion point; the
            anchor punctuation differs per language — TS `: T` vs Rust `-> T`).
      - [x] **Comment / docstring edits (done).** Each symbol carries a `:doc` sub-node anchor —
            the leading comment / JSDoc (TS), `///`/`/** */` doc comments (Rust), or the docstring
            (Python). Edit it with the existing verbs: `replace_node target:"doc"` (rewrite),
            `replace_text target:"doc"` (tweak), or `insert_before` (add where none exists). Safe
            by construction (comment edits introduce no type errors, so the gate passes trivially).
            Follow-up: Rust line-comment `:doc` ranges include the trailing newline (replacement
            should include one); doc separated from the item by an attribute isn't captured yet.
- [ ] **Config providers (JSON/YAML/TOML)** — tree-sitter providers for surgical key edits
      (package.json, compose, *.toml); no gate needed. Rides on the provider registry.
- [x] **Tree-sitter fallback edit provider (done, v0 — Python).** `lang-fallback` crate: a
      tree-sitter `LanguageProvider` for languages without a SCIP/LSP integration yet. Read path
      — `structure()` (functions/classes/methods + fn sub-nodes), `import_graph()`
      (`import` / `from … import` resolution incl. relative dots), skeletal `outline()` (bodies
      folded to `...`) — all in-process, no external tooling. **Ungated** structural edits
      (`replace_node` / `insert_before` / `create` / `move` / `delete`, plus a best-effort
      within-file `rename`) through the same VFS/blast-radius/atomic-commit path as the gated
      providers, behind a no-op `GateEngine`. The MCP success message reports **`gated: false`**
      so the agent knows the edit is structural, not type-checked. Dispatched by `FbLang::detect`
      (`.py` present) / `CI_LANG=python`; indexes + retrieves a Python repo end-to-end.
      - [ ] **Go** (and Ruby/…) — a data addition to `FbLang` (grammar + the few node-kind names);
            the provider, dispatch, outline, and ungated edit path are already language-generic.
      - [ ] Per-language **upgrade to the gated path** as each LSP/indexer lands (pyright + a
            SCIP-python indexer for Python; gopls + scip-go for Go) — swap the no-op gate for the
            real `GateEngine`, reaching capability parity with TS/Rust.
### Requirements for ANY new language provider (the capability checklist)

This is the bar TS and Rust now meet — **every new provider should target all of it.** The seams
(`LanguageProvider`, `GateEngine`, per-crate `outline`) make most of it *wiring*, not core work.
Treat this as the definition-of-done when adding a language; don't ship a read-only language
without a path to its edit gate.

**Read**
- [ ] `structure()` — symbols (fns / methods / types / fields) AND their **sub-node anchors**:
      `:param.N`, `:return`, `:body`, **`:doc`** (leading comment / docstring). Field/variable
      ranges must span the full declaration, not just the name.
- [ ] `import_graph()` — the language's real dependency edges (imports / `mod` / `use` / `from`).
- [ ] `outline()` — skeletal context: fold function/method bodies, keep signatures + structure.

**Write** (all atomic + through the blast-radius gate)
- [ ] Structural: `rename`, `replace_node`, `move_file` (+ create / delete / insert_before).
- [ ] Surgical sub-node: `set_body`, `replace_node target:body|return|param.N|doc`, `replace_text`.
- [ ] **Gate:** a real `GateEngine` (LSP/indexer) so edits are type-checked; until one exists, the
      tree-sitter fallback path applies them **ungated** and the result says `gated: false`.

**Dispatch**
- [ ] Manifest/extension detection in `build_provider`/`select_provider` (+ `CI_LANG` override);
      a `.ext → outline` arm in the MCP `outline_for`.

Per-language status: **TS ✅ · Rust ✅ · Python** (read + outline + ungated edits ✅; gated path
pending — pyright/scip-python) · **Go/Ruby/…** (fallback is a data addition to `FbLang`).

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
- **Dogfooding:** once Marksman can index and edit Rust, we use *it* to build the remaining
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
- **Fine verbs over the AST tree** — `set_body` + `replace_node target:body|return|param.N` are
  mapped in `action_to_op` over the `#sym:body` / `:return` / `:param.N` anchors and shipped
  (see the gated "Surgical sub-node edits" backlog item). Remaining: statement-level body edits,
  `add_parameter`/`set_return_type`-where-absent, and non-ASCII (byte-based) column handling.
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
  body + byte-vs-char column handling. **Marksman's edge over a pure-AST editor: these stay
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
(SCIP/LSP — type-checked, few languages). Marksman can do *both* through the existing
`Granularity` + `GateEngine` seams:
- A **tree-sitter structural-edit provider with no blast-radius gate** is the *fallback* for any
  language we don't yet have SCIP/LSP for (Python, Go, Ruby, …) — structural edits work
  immediately, best-effort.
- Per language, upgrade to the **gated** path as the SCIP/LSP integration lands (Rust first).
- So a multi-language repo is *useful on day one*, and the type-checked guarantee is layered in
  where the toolchain exists. **Honest tradeoff:** tree-sitter edits aren't type-checked — the
  result must say so (e.g. `gated: false`) so the agent knows it's structural, not verified.
