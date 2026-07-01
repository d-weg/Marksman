# Marksman — roadmap

Directions **with a delivery plan.** The open work is organized into **batches** you execute one at
a time — same rhythm as [code-review-plan.md](code-review-plan.md): read the seams, make the
smallest safe change, keep the suite green, one commit per batch, tick the boxes here.

## How to run a batch
1. Read the files/seams the batch names; jot the concrete change.
2. Implement the smallest change that satisfies each item; add a test for every new branch.
3. `cargo test --workspace` green + `cargo clippy --workspace --all-targets` clean.
4. One commit per batch (`feat(<area>): …` / `fix(<area>): …`); update the checkboxes + a one-line
   note here. Optionally `/code-review` the diff first.

## Invariants (the bar every batch holds)
- **Pure-Rust core.** Indexing, the Model2Vec embedder, BM25, RRF, retrieval, the VFS, and the
  edit-gate *orchestration* need nothing but Rust. Any runtime dependency (Node, an indexer, a
  language server) belongs to a **language provider**, never the core. Load-bearing.
- **Gated edits stay gated.** Every code edit is type-checked over the blast radius before it lands;
  the tree-sitter fallback applies edits **ungated** and must report `gated: false`.
- **Address by handle.** A node id (`file#Scope.name`, optionally `:body`/`:doc`/`:param.N`) is
  unique AND self-locating — resolve cheapest-precision first, never make the agent re-locate.
- **No ranking change without an eval.** Once the labeled eval lands (Batch 5), any change to a
  retrieval weight must show a non-regression against it — no more tuning by feel.

## North star: pure-Rust core; languages as modular, on-demand providers
The core runs on Rust alone; each language provider declares its own toolchain and **fetches it
lazily, only when a file of that language is indexed**. A Rust-only repo never invokes Node; a repo
with no TypeScript never fetches `scip-typescript`/`ts-morph`. `build_index` dispatches per file
through an **extension → provider registry** (Batch 6), so a mixed repo indexes every language and
Node is only touched when `.ts*` files are present; the remaining gap is that the embedder model
isn't lazily fetched (Batch 3).

**Chosen distribution model:** providers live in-repo under `crates/langs/`, each with a
`marksman-provider-<lang>` bin; one `cargo build` produces them all, and `CI_PROVIDER=sidecar`
spawns the one a repo needs (resolved next to the exe). Adding a language is a new folder in
`langs/` — no download system. Downloadable/third-party providers stay deferred until there's a
reason (a slim core, or externally-published providers).

## Shipped ✅ (the baseline these batches build on)
- **Rust provider** — read (structure + sub-nodes + `mod`/`use` graph, in-process `tree-sitter-rust`)
  and write (rename / replace_node / move_file, type-checked via rust-analyzer). Opt-in
  compiler-accurate `use` graph from cached `rust-analyzer scip` (`scip.rust` / `CI_SCIP_RUST`).
- **TypeScript provider** — SCIP (`scip-typescript`) read + ts-morph/LSP gated write.
- **Tree-sitter fallback (Python)** — read + outline + **ungated** structural edits (`gated: false`).
- **Provider selection v0** — manifest/extension pick (Cargo.toml vs package.json; `CI_LANG`); Node
  only for a TS repo.
- **Sidecar protocol** — `ci-proto` (protobuf wire + framing), `ProcessProvider` host, and
  `marksman-provider-{rust,ts}` sidecars; `CI_PROVIDER=sidecar` routes CLI/MCP over the wire.
- **Skeletal context** — `retrieve_context detailLevel` (`pointers`/`outline`/`full`), secondary
  import-graph files auto-fold to outline, plus the `read_node` drill-down tool.
- **Surgical sub-node edits** — `set_body`, `replace_node target:body|return|param.N|doc`,
  `replace_text` (unique substring), and `:doc` comment/docstring edits — all through the gate.
- **Addressing model** — id ≫ name-in-file ≫ name-in-index ≫ free-text `query`; collisions return
  candidate ids. `retrieve_and_edit` (query→edit in one gated call).
- **Shared internals** — `ci-treesitter` (ts_range / syntax_node / leading_comment / body_ranges),
  `reverse_import_map`, `forward_adjacency` (from the code-quality pass).

---

## Batches (priority order — correctness & safety first, then value, then breadth)

### Batch 1 — Write & index safety (the trust boundary)  ✅
**Why:** an autonomous, possibly prompt-injected agent drives this. `create_file`/`move_file`/
`delete_file` took arbitrary paths joined to root with **no containment check**; `save_index` wrote
six files **in place, sequentially** (a crash or concurrent read yielded a torn index whose vectors
no longer align with chunks).
- [x] Root-containment guard (`ci_edit::ensure_within_root`): rejects any op whose path escapes root
      — lexical (`..`/absolute) + symlink (canonical-ancestor under root). Checked in `commit_edits`
      before the VFS mutates; returns `Rejected`, never writes.
- [x] Atomic index write: `save_index` serializes into a sibling temp dir then `rename`s into place,
      so a reader never sees a half-written index (previous index restored on swap failure).
- [x] Single-writer `IndexLock` (self-healing `.<name>.lock`, stolen after 15 min stale) so a CLI
      `index` can't interleave with the server's reindex.
- [x] Tests: `create_file path:"../x"|"/etc/passwd"` rejected (in-repo create still commits); save
      leaves no temp/lock artifacts and reloads; lock is exclusive then released on drop.

### Batch 2 — Index lifecycle: keep the index true after edits  ✅
**Why:** `apply_edits` committed to disk but nothing reindexed — `ci_build::update_index` was defined
and **never called**, so the index (symbols / graph / vectors / BM25) went stale the moment an agent
made its first edit, and any follow-up `retrieve_context`/`list_anchors`/name-resolution reasoned
against the pre-edit world.
- [x] `Server::reindex_after_edit` — after a written commit, incrementally reindex the returned
      `changed_files` via `ci_build::update_index` and atomic-save (Batch 1). A reindex hiccup logs
      and leaves the (already-committed) edit intact rather than failing it.
- [x] load → `update_index` → save, reusing the server's provider + embedder.
- [x] `load_index` refuses a `meta.version != INDEX_VERSION` index with a "re-run index" hint;
      `build_index` now stamps `ci_index::INDEX_VERSION` (one source of truth).
- [x] Tests: version mismatch rejected (ci-index); `update_index → save → reload` reflects the edit
      (new symbol in, stale out, vectors row-aligned) (ci-build). Session-level MCP e2e (edit then
      retrieve) stays an `#[ignore]` integration concern — it needs the embedding model + a live
      provider; the wiring is a straight compose of these component-tested pieces.

### Batch 3 — Provisioning parity (embedder + schema)  ✅
**Why:** the lazy-fetch invariant applied to provider tooling but **not** the embedding model, which
was a manual `git clone … ~/.marksman/models`; and a query embedded with the wrong dim could panic
`cosine_normalized`.
- [x] `ci_embed::ensure_model` — lazy-fetches the Model2Vec files from HuggingFace via `curl` on
      first use (same lazy-tooling model as the providers), no-op when present; `CI_MODEL_DIR` still
      overrides; on failure/offline (or `CI_NO_MODEL_FETCH`) returns a precise error with the manual
      command. Called by the CLI (`index`/`retrieve`) and the MCP `embedder()`.
- [x] `ensure_index_matches` guard (ci-mcp `retrieve_context`/`resolve_query`, and ci-cli
      `retrieve`): the index's `meta.model`/`meta.dims` must match the active embedder, else a clear
      "re-run index" error — closes the `cosine_normalized` out-of-bounds panic from the review
      notes and prevents a silent cross-model mis-rank. (`IndexMeta` already records model + dims.)
- [x] Tests: `ensure_model` no-op when present / actionable error when absent+`CI_NO_MODEL_FETCH`;
      `ensure_index_matches` accepts a match and rejects dim/model mismatch.

### Batch 4 — `find_symbols`: keyword/symbol search that returns handles  ✅
**Why:** filled the gap between `retrieve_context` (fuzzy, concept→files) and grep (literal, but
returns lines the agent must map back to symbols). Every hit is a self-locating handle, so the next
step is `read_node id=…` / `apply_edits name=…` with no re-derivation.
- [x] `ci_retrieve::find_symbols` + the `find_symbols` MCP tool: exact/substring match over indexed
      symbol names, returning node-id handles + kind + range, ranked exact-first then by path-role/
      layer weight then id. Exhaustive by default (audits), truncated to 200 with the total noted.
- [x] `MatchedSym` now carries `node_id`; `retrieve_context`'s matched-symbol lines print the handle
      (`[file#Scope.name]`) in both the MCP and CLI renderers.
- [x] The `find_symbols` + `read_node` tool text points at `read_node id=…:body`/`:doc` so a body
      edit loads only the body.
- [x] Extracted `ci_retrieve::file_weighter` (was inline in `retrieve`) so ranking is shared, not
      duplicated. Tests: exact→one handle, substring→all (docs excluded), cap truncates but total
      still counts every match.

### Batch 5 — Ranking evaluation + multi-language retrieval weighting  ✅
**Why:** retrieval weights (`rrf_k`, `symbol_match_bonus`, the layer boost) are hand-tuned with **no
labeled eval** to catch a regression, and role/layer signals are **npm/tsconfig-centric** — a Rust
or Python repo gets degraded weighting even once indexing is multi-language.
- [x] **5a — cross-language path/query vocabularies (done).** `segment_role` now classifies
      Django/DRF + DDD dirs (`serializers`/`repositories`/`usecases`/`entities`/…) as backend, and
      `default_layer_terms` fires on Rust/Python framework+ORM query terms (axum/sqlx/tokio/tonic,
      django/flask/fastapi/sqlalchemy/celery/…). This is the **active** signal (path-role + query
      layer boost already work cross-language), so Rust/Python repos now get real layer weighting.
      Tested (path roles + layer firing across languages).
- [x] **5b — dep-based persisted roles (done).** `detect_workspace` now discovers `package.json`,
      `Cargo.toml`, and `pyproject.toml` (a bare Cargo `[workspace]` root is not a package) and
      extracts each manifest's dep names (via `toml` + serde_json). `build_index` runs `infer_role`
      over those deps and persists the result in `PackageMeta.role`; `retrieve`'s `file_weighter`
      feeds it to `resolve_role` (which prefers a persisted role). Dep fingerprints extended with
      Rust (axum/actix/rocket/sqlx/diesel/sea-orm, leptos/yew) and Python (django/flask/fastapi/
      sqlalchemy/celery/…). Tested: Cargo+pyproject dep parsing, `[workspace]`-root skip, and a
      crate with an `axum` dep persisting `role: "backend"`.
- [x] **5c — labeled eval harness (done).** `codeindex-rs eval <root> <eval.json> [--top N]` runs a
      labeled set (`{task, expectFiles}`) against `retrieve` and reports overlap@k + MRR — the gate
      for any future weight change (see Invariants). Scoring (`score_case`: reciprocal rank + hit@k)
      is a pure, unit-tested function; a seed set on Marksman's own crates lives at
      [docs/eval/marksman.json](eval/marksman.json). (Wiring `scripts/agent-bench` to consume it is
      follow-up.)
- [ ] (ref) the three-way + agent A/B benchmark design lives in [benchmarks.md](benchmarks.md).

### Batch 6 — Provider registry (multi-language repos)  ✅ done
**Why:** indexing/editing still bind **one** provider per repo, so a mixed Rust+TS+Python repo can't
be fully indexed, and tooling isn't fetched per-language.

**Scope note (settled — don't re-litigate):** this is *smaller* than "an architecture rework." BM25,
embeddings, RRF, symbol search, and graph expansion all already operate on ONE unified index and are
language-blind — so once every file is indexed, cross-language retrieval fuses for free; retrieval
needs **zero changes**. Import edges are essentially always *within*-language (a TS↔Rust call is a
network boundary, not a syntactic import, and no provider emits an edge for it), and each provider
already scopes its graph to its own files — so combining graphs is a trivial **union**
(`forward_adjacency` per provider, extend into one map), NOT a cross-language merge. The real work is
per-file dispatch at index time.
- [x] `extension → provider` registry (`ci_build::ProviderRegistry`); `build_index`/`update_index`
      pick the provider per file by extension via `ci_walk::Lang::of`, so a mixed Rust+TS+Python repo
      indexes fully. Retrieval was already language-blind, so it needed zero changes. The MCP output
      tools (`list_anchors`/`read_node`/`apply_edits`) dispatch per file through the same registry.
- [x] Union the per-provider `import_graph()`s in `forward_adjacency` (each provider scopes its graph
      to its own files, so the file keys are disjoint across languages — a plain union, verified with
      a mixed repo whose graph carries both the Rust `mod` edge and the Python `from`-import edge).
- [x] Lazy per-language tooling fetch: `ci_build::build_registry` detects present languages
      (`ci_walk::present_langs`) and only constructs a provider for each, so a repo with no `.ts*`
      never runs `scip-typescript`/Node. `CI_LANG` still forces a single language.
- [x] Provider manifest (`config.providers.<lang>`): `enabled` gates a language out of the registry
      (verified — a repo can turn Python indexing off); `bin` points at a vendored sidecar binary
      (highest-priority in `sidecar_command_with` — the offline/air-gapped path); `version` pins a
      tool version. Env override `CI_PROVIDER_<LANG>_ENABLED`.

### Batch 7 — Deeper edits + structured non-code providers  ✅ done
**Why:** extend surgical editing to single statements and to the config/data/docs files agents touch
alongside code.
- [x] Statement-level body edits: `insert_in_body` (append, or after a unique body line) /
      `delete_in_body` (remove a unique statement line) — pure text surgery on the `:body` sub-node
      in `ci-edit::apply_structural`, so it's language-generic (brace block or Python suite; a
      suite's missing first-line indent is supplied by the body's start column). Wired through
      `action_to_op`, the protobuf sidecar, and the MCP `apply_edits` schema.
- [x] `add_parameter` (append to the `:params` `(...)` list, before `)`) / `set_return_type` at the
      language's insertion point where none exists (after `)`: TS `: T`, Rust/Python `-> T`; refused
      when a return type already exists — use `replace_node target:return`). Providers now emit a
      `:params` sub-node as the anchor. Tested end-to-end through the ungated Python provider.
- [x] Non-ASCII (byte-vs-char) column handling across the edit path; deduped `ci-vfs::byte_offset`
      / `lang-ts::point_byte` into one `ci_core::text::byte_offset`. Fixed the real bug: tree-sitter
      `Point.column` is a UTF-8 *byte* offset, but the shared helper counted Unicode scalars — so a
      sub-node edit on a line with a multi-byte char before it mis-resolved. The `Range` column
      contract is now documented as 0-based UTF-8 bytes (matches tree-sitter + the VFS); non-ASCII
      tests in `ci-core` and `ci-vfs`. (SCIP/LSP UTF-16 boundaries convert at their own edges.)
- [x] Structured providers (**TOML / JSON / YAML / Markdown**): `set_key` / `delete_key` in
      `ci-edit::structured`, edit by structural **key** (dotted path for TOML/JSON/YAML) or **heading
      path** (`/`-nested Markdown section). No reformatting — TOML via `toml_edit`; JSON via a small
      byte-span scanner (no dependency; `serde_json` validates the result so a splice can never write
      corrupt JSON); YAML line-based (block mappings only — the common config shape; flow style /
      anchors out of scope); Markdown by section. **Ungated** (the gate excludes structured files; a
      structured-only batch never boots a language server) and applied in the **same atomic batch**
      as the code edit via `commit_edits` — proven end-to-end (a `Cargo.toml` dep beside a Python
      `set_body`, one commit).

### Batch 8 — Breadth: more languages + retrieval scale
**Why:** last, once the lifecycle / safety / quality floor is in.
- [ ] Go / Ruby fallback: a data addition to `FbLang` (grammar + node-kind names); the provider,
      dispatch, outline, and ungated-edit path are already language-generic.
- [ ] Gated upgrades per language: pyright + scip-python (Python), gopls + scip-go (Go) — swap the
      no-op gate for a real `GateEngine`, reaching TS/Rust parity.
- [ ] Retrieval scale: decide an ANN/inverted index **or** an explicit "small/medium repos" non-goal
      plus a file cap — BM25 search and vector ranking are both O(n) per query today (fine now,
      unbounded on a large monorepo; `ci-arch` already caps at 20k files, the index doesn't).

## Capability checklist for ANY new language provider (definition-of-done)
The bar TS and Rust meet — every new provider should target all of it. The seams
(`LanguageProvider`, `GateEngine`, per-crate `outline`) make most of it *wiring*, not core work.
Don't ship a read-only language without a path to its edit gate.

**Read**
- [ ] `structure()` — symbols (fns / methods / types / fields) AND sub-node anchors `:param.N`,
      `:return`, `:body`, `:doc`. Field/variable ranges span the full declaration, not just the name.
- [ ] `import_graph()` — the language's real dependency edges (import / `mod` / `use` / `from`).
- [ ] `outline()` — skeletal context: fold function/method bodies, keep signatures + structure.

**Write** (all atomic + through the blast-radius gate)
- [ ] Structural: `rename`, `replace_node`, `move_file` (+ create / delete / insert_before).
- [ ] Surgical sub-node: `set_body`, `replace_node target:body|return|param.N|doc`, `replace_text`.
- [ ] **Gate:** a real `GateEngine` (LSP/indexer) so edits are type-checked; until one exists, the
      fallback applies them **ungated** and the result says `gated: false`.

**Dispatch**
- [ ] Manifest/extension detection in the provider registry (+ `CI_LANG`); a `.ext → outline` arm
      in the MCP `outline_for`.

Per-language status: **TS ✅ · Rust ✅ · Python** (read + outline + ungated edits ✅; gated path
pending — Batch 8) · **Go/Ruby/…** (fallback is a data addition to `FbLang` — Batch 8).

## Done = every open box above checked, suite green, clippy clean, and this file's notes updated.
