# Marksman — roadmap

Directions with a delivery plan: the open work is organized into **batches**, executed one at a
time — smallest safe change, suite stays green, a test for every new branch, one commit per
batch, checkboxes ticked here.

## Invariants (the bar every batch holds)
- **Code only.** Marksman indexes and edits **source code** — nothing else. No docs (`.md`), no
  config/data (`.toml`/`.json`/`.yaml`). Code already has richer structural editing (AST-anchored,
  gated `apply_edits`) than any key-path editor could give a data file, and non-code files are a
  different domain agents handle with plain text tools. Keeps the surface (and the MCP tool schema)
  focused. The only "doc" that stays is a code symbol's own leading comment/docstring (`:doc`).
- **Pure-Rust core.** Indexing, the Model2Vec embedder, BM25, RRF, retrieval, the VFS, and the
  edit-gate *orchestration* need nothing but Rust. Any runtime dependency (Node, an indexer, a
  language server) belongs to a **language provider**, never the core. Load-bearing.
- **Gated edits stay gated.** Every code edit is type-checked over the blast radius before it lands;
  the tree-sitter fallback applies edits **syntax-gated only** (new parse errors reject; no type
  check) and must report `gated: false`.
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
- [x] **5c — labeled eval harness (done).** `marksman eval <root> <eval.json> [--top N]` runs a
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
- ~~Structured providers (TOML/JSON/YAML/Markdown): edit config/docs by key / heading path in the
      same atomic batch as code.~~ **Cut — deliberately descoped.** Marksman is a **code-only** tool:
      non-code files aren't code, and code already has richer structural editing (AST-anchored,
      gated `apply_edits`). `set_key`/`delete_key` added surface area (a parser module, two edit ops,
      MCP-schema tokens paid every turn) for a niche "edit a `Cargo.toml` dep atomically with the
      code" workflow agents handle fine with plain text edits. Removed; the `set_key` machinery is
      not useful for code (code's "path" is the node id, with type-checking on top).

### Batch 8 — Breadth: more languages, via the measured rollout ladder
**Why:** last, once the lifecycle / safety / quality floor is in. **The ladder is now
measured policy** (read-path ablation + T9/T10, [benchmarks.md](benchmarks.md) §2–3): the
compiler GATE carries the agent value (tree-sitter reads + a real gate = −36% vs baseline,
zero startup deps); SCIP is the *maturity step* that keeps the gate's radius sound on
monorepos / cross-package imports (where its edges are the entire measured advantage). So a
new language does NOT wait for a scip indexer:
- [x] Go / Ruby / Java / C / C++ / JS fallback: data additions to `FbLang` (grammar + classify
      rows); provider, dispatch, outline, syntax-gated edits, and the rename verification scan
      are language-generic.
- [ ] **Step 1 — tree-sitter + gate** per language: keep the generic tree-sitter read + the
      syntactic import graph (served TRANSITIVELY to the gate — `transitive_reverse_imports`,
      the T9 fix) and swap `NoGate` for the language's real checker as `GateEngine`: pyright
      (Python), gopls (Go), and JS through tsc (only honest with `allowJs`+`checkJs` handling —
      a weak gate must not claim "type-checked clean"). `TsTreeGated` is the proven template.
- [ ] **Step 2 — scip indexer** per language (scip-python, scip-go, …), when that language's
      users hit the seams where it is load-bearing: cross-package/bare-specifier imports (the
      syntactic graph has NO such edges — T10) and radius precision on large repos (the
      transitive closure over-approximates; scip's one-hop set stays bounded by true
      referencers).
- [ ] Retrieval scale: decide an ANN/inverted index **or** an explicit "small/medium repos" non-goal
      plus a file cap — BM25 search and vector ranking are both O(n) per query today (fine now,
      unbounded on a large monorepo; `ci-arch` already caps at 20k files, the index doesn't).

### Batch 9 — Graph-centrality retrieval prior (PageRank over the import graph)
**Why:** retrieval fuses three *query-relevance* signals (vector · BM25 · symbol) and then multiplies
by a *static* prior (`file_weighter`: package weight × query-conditioned layer boost). It has **no
signal for structural importance** — that a file is a foundational, widely-imported module vs. a
leaf. The import graph already encodes this (it's in `index.graph` at query time), but today it's
only used *reactively* (1-hop `adjacency_to_seeds` bonus, N-hop `expand_graph`), never as a global
prior on the file itself. Adding graph centrality as a second multiplicative prior lets a file that
is *both query-relevant and structurally central* outrank an equally-relevant leaf — the same
PageRank-ranked-repo-map idea aider and AGF (graph-flow.cloud) use, but fused into our existing RRF
pipeline and **gated by the labeled eval** (Batch 5c), not shipped by feel.

**Design note (settled — don't re-litigate):** centrality is a **multiplicative prior on the fused
score, NOT a fourth RRF list.** A standalone centrality list would seed pure hub files (a barrel /
`types.ts` everything imports) on *every* query regardless of relevance — the exact failure the
`exact_symbol_match_outranks_adjacency_hub` test guards against. Multiplying `weighted_fused`
(which only contains files RRF already surfaced from a query-relevance search) means a file with
**zero** query signal is absent from `fused` and can never be lifted into seeds by centrality alone;
centrality only re-ranks *among already-relevant files*. The large additive `symbol_match_bonus`
(step 6) still applies on top, so a named leaf **definition** stays ahead of a central hub. The two
existing hub/expansion tests plus the eval are the gate.

- [ ] **9a — PageRank kernel (pure, in `ci-index`).** New `ci_index::centrality` module: PageRank
      over `Adjacency` (forward edges = "imports"; a file with many *incoming* forward edges is
      widely-imported → high score). Standard `d=0.85`, dangling-mass redistribution, ~30 iters or
      L1 Δ < 1e-6; node set = every file in `meta.files` (isolated files get the `(1-d)/N` floor).
      Language-blind (operates on the unioned per-provider graph from Batch 6). Returns
      `BTreeMap<String, f32>` (sums≈1). Tests: a file imported by 3 others outranks a leaf; isolated
      node gets the floor; deterministic; mass conserved.
- [ ] **9b — persist at index time.** Compute centrality in `ci_build::build_index` after the graph
      is assembled, store on `IndexData` (+ atomic-save/load, Batch 1); recompute in `update_index`
      after an edit changes the graph (Batch 2). Bump `INDEX_VERSION` (stale indexes already refuse
      with a re-index hint — Batch 2/3). *Rationale for persist-vs-recompute:* centrality is a global
      property that only changes on an edit (which already triggers reindex), so precompute once and
      cache it (matches the "input is cacheable" ethos) instead of recomputing every query; it also
      makes the values reusable by `describe_architecture` (9d). Alternative (query-time compute in
      `ci-retrieve`, no format bump) is smaller but recomputes per query — rejected as the primary.
- [ ] **9c — fuse as a prior + eval-gated default.** New `Config.centrality_weight: f32`
      (`#[serde(default)]`). In `retrieve`, fold a bounded, **rank-normalized** centrality multiplier
      (`1.0 + centrality_weight · normrank(pr[file])`, `normrank`∈[0,1] over the candidate set — rank
      is robust to one hub dominating raw PR) into the same `weighted_fused` multiply that applies
      `weight_for`. **Per the "no ranking change without an eval" invariant:** run `marksman eval
      <root> docs/eval/marksman.json` at weight ∈ {0, 0.1, 0.2, 0.3}; commit the default that
      maximizes overlap@k + MRR **without regressing** baseline. If nothing beats weight=0, ship the
      mechanism with default `0.0` (off) + a note — the knob still helps hub-heavy repos and the
      invariant (no regression) holds. Tests: hub/expansion tests stay green; a synthetic case where
      two equally query-relevant files differ *only* in centrality ranks the central one first when
      `centrality_weight > 0`.
- [ ] **9d — (stretch) surface core modules in `describe_architecture`.** Reuse the persisted
      centrality to tag/sort each directory's top files as the repo's "core modules" — a token-budgeted
      ranked map (AGF's ~1k-token repo map), reusing 9b, no recompute. Keep it a data addition to the
      `ci-arch` output; defer if it grows the schema.

### Provider conformance — one contract, verified across every provider  ✅ (pre-OSS gate, shipped)
**Why:** providers span three tiers (full/scip · tree-sitter+gate · ungated fallback) and
eight+ languages, and the ablation work proved the failure mode to fear: a provider that
*mostly* behaves but quietly diverges on a contract point (a one-hop radius where transitivity
is required, a stale read after commit, a "clean" claim its gate can't back). The contract is
now executable — the NEXT provider can't ship without passing it.
- [x] **The contract**: [docs/provider-contract.md](provider-contract.md) — identity/addressing,
      reads, graph semantics (syntactic graphs served TRANSITIVELY to the gate), edit atomicity,
      gate soundness (barrel + cross-package cases named), honesty, freshness, code consistency.
- [x] **The suite**: `crates/ci-conformance` — one shared battery (read invariants: id scheme,
      nesting, name_range slicing, `:body`/`:params`/`:doc` anchors, graph determinism/honesty;
      edit clauses: commit, dry-run, syntax-gate reject, whole-batch atomicity, soft-fail
      anchors) × per-language fixtures. Fast tier in CI (fallback×8 + rust reads); `--ignored`
      tier for scip-typescript. Live-checker gate e2es remain in the provider crates as
      contract instances (barrel, monorepo, rename/reject suites), referenced from the doc.
- [x] **The audit**: all 10 instances green; grep-audit confirms every provider routes writes
      through `commit_edits` and no provider re-implements the graph/text helpers. CONTRIBUTING
      wires the suite into the PR flow.
- [x] `lang-template` skeleton crate: `GatedTreeSitter` (tree-sitter reads + a pluggable
      checker on the shared spine, transitive radius wired) with a mock checker and its OWN
      conformance instance — a copied crate starts conformant, provably. Step-1 languages
      (Batch 8) start here.

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
