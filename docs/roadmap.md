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
with no TypeScript never fetches `scip-typescript`/`ts-morph`. `build_index` is already generic over
`LanguageProvider` and Node is confined to `lang-ts` — the gap is that entry points still pick **one
provider per repo** (Batch 6) and the embedder model isn't lazily fetched (Batch 3).

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

### Batch 1 — Write & index safety (the trust boundary)
**Why:** an autonomous, possibly prompt-injected agent drives this. `create_file`/`move_file`/
`delete_file` take arbitrary paths joined to root with **no containment check**; `save_index` writes
six files **in place, sequentially** (a crash or concurrent read yields a torn index whose vectors
no longer align with chunks).
- [ ] Root-containment guard: reject any create/move/delete whose resolved path escapes root
      (`..`, absolute, symlink-out). One check in `ci-edit` before the VFS mutates.
- [ ] Atomic index write: write into a temp dir under the index dir, then rename into place, so a
      reader never sees a half-written index.
- [ ] Single-writer lock (lockfile in the index dir) so a reindex can't race a read or another writer.
- [ ] Tests: `create_file path:"../x"` rejected; an interrupted save leaves the prior index intact.

### Batch 2 — Index lifecycle: keep the index true after edits
**Why:** `apply_edits` commits to disk but nothing reindexes — `ci_build::update_index` is defined
and **never called**, so the index (symbols / graph / vectors / BM25) is stale the moment an agent
makes its first edit, and any follow-up `retrieve_context`/`list_anchors`/name-resolution reasons
against the pre-edit world. Close the loop.
- [ ] After a successful `apply_edits` commit, incrementally reindex the returned `changed_files`
      via `ci_build::update_index` and persist — in `ci-mcp` (and the sidecar `apply_edits` path).
- [ ] Reuse the in-memory index where the server already holds one; else load → update → save.
- [ ] Check `IndexMeta.version` on load and rebuild/refuse a mismatched schema instead of
      mis-reading an old layout (the field exists; nothing reads it).
- [ ] Test: edit a symbol, then `list_anchors`/`retrieve_context` reflect the new state same session.

### Batch 3 — Provisioning parity (embedder + schema)
**Why:** the lazy-fetch invariant applies to provider tooling but **not** the embedding model, which
is a manual `git clone … ~/.marksman/models`. Make the embedder obey the same rule.
- [ ] Lazy-fetch + cache the Model2Vec model on first index (per the provider lazy-tooling model);
      `CI_MODEL_DIR` still overrides; a clear error + one-line fetch hint when offline.
- [ ] Record model id + dims in `IndexMeta`; a query embedded with a different model/dim is a clear
      error (ties into Batch 2's version check) — never a silent mis-rank or an out-of-bounds panic
      in `cosine_normalized` (see code-review-plan deferred notes).

### Batch 4 — `find_symbols`: keyword/symbol search that returns handles
**Why:** fills the gap between `retrieve_context` (fuzzy, concept→files) and grep (literal, but
returns lines the agent must map back to symbols). Every hit is a self-locating handle, so the next
step is `read_node id=…` / `apply_edits name=…` with no re-derivation. Pays off even for a lean
single-server agent — the handle-return is the point.
- [ ] `find_symbols` MCP tool: exact/substring match over indexed symbol names (opt. comments),
      returning `{node_id, kind, range, weight}` ranked by the path-role/layer weights — **not**
      `file:line`. Exact + exhaustive by default (audits: "every impl of X"), not top-k; cap + note
      when huge.
- [ ] Surface the qualified node id in `retrieve_context` matched-symbol lines too, so the handle
      propagates directly instead of being reconstructed.
- [ ] Encourage sub-node reads (`read_node id=…:body`/`:doc`) so a body edit loads only the body.
- [ ] Tests: unique name → one handle; ambiguous → all handles; substring mode.

### Batch 5 — Ranking evaluation + multi-language retrieval weighting
**Why:** retrieval weights (`rrf_k`, `symbol_match_bonus`, the layer boost) are hand-tuned with **no
labeled eval** to catch a regression, and role/layer signals are **npm/tsconfig-centric** — a Rust
or Python repo gets degraded weighting even once indexing is multi-language.
- [ ] A labeled eval set (task → expected files/symbols) + a runner reporting manifest overlap +
      rank; wire `scripts/agent-bench` as the harness. This becomes the gate for any weight change.
- [ ] Role/layer fingerprints beyond npm: Cargo (`[dependencies]`, workspace members) and Python
      (pyproject/requirements) so `infer_role` classifies non-TS packages.
- [ ] Persisted package roles (deps-based `infer_role` at index time) for sharper query weighting.
- [ ] (ref) the three-way + agent A/B benchmark design lives in [benchmarks.md](benchmarks.md).

### Batch 6 — Provider registry (multi-language repos)
**Why:** indexing/editing still bind **one** provider per repo, so a mixed Rust+TS+Python repo can't
be fully indexed, and tooling isn't fetched per-language.
- [ ] `extension → provider` registry; `index` derives the active set from files present and routes
      each file through its owning provider (multi-language repos work; Node touched only if `.ts*`).
- [ ] Lazy per-language tooling fetch, cached per provider; nothing fetched for absent languages.
- [ ] Provider manifest: enable/disable a language, pin a tool version, point at a vendored binary
      (offline/air-gapped).

### Batch 7 — Deeper edits + structured non-code providers
**Why:** extend surgical editing to single statements and to the config/data/docs files agents touch
alongside code.
- [ ] Statement-level body edits: `insert_in_body` / `delete_in_body` (one statement in a block;
      replace-in-body is already covered by `replace_text target:"body"`).
- [ ] `add_parameter` / `set_return_type` where **no** anchor exists (params-end / return insertion
      point; TS `: T` vs Rust `-> T`).
- [ ] Non-ASCII (byte-vs-char) column handling across the edit path; while here, dedupe the
      near-identical `ci-vfs::byte_offset` / `lang-ts::point_byte` into a `ci-core` util.
- [ ] Structured providers (JSON/YAML/TOML/Markdown): edit by structural **key** / heading path — no
      reformatting, no gate (no type-check) — in the **same atomic batch** as the code edit that
      needs them (e.g. a `package.json`/`Cargo.toml` dep beside the import that uses it).

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
