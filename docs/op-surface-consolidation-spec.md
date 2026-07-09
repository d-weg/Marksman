# Op-surface consolidation — spec for review

**Status: EXECUTED (2026-07-06) — P1-P5, P7, P8, P11, and P6 (rust-first, then ts) are
implemented and verified green (workspace build zero-warning, full non-ignored suite, all
real-tool `#[ignore]` e2e batteries). Decisions recorded: F9 → option (a) (delete the
hidden arms). The P6 bench spot-check ran 2026-07-06 (full sweep, both suites, both arms,
1 run/cell, same model as the published table): **no freshness regression** — Marksman-arm
turn counts equal the published §1 medians in 10/12 comparable cells, `move-rust` improved
(4→3 calls, $0.0712→$0.0466), and the one over-median cell (`body-edit-ts`, 5→7) is an
agent-side placement correction whose transcript *demonstrates* the freshness channel
working (its post-commit re-inspect saw the committed edit and enabled the fix). A uniform
~+10% $ drift on turn-identical cells is attributed to tool-description growth from the
already-committed add_symbol work (post-dates the published table, pre-dates P6).
P9/P10 stay deferred to their triggers. The `Composed::rel()` leftover is fixed
(delegates to `ci_core::rel_path`) and the §7 audit's banned-outright rule now also sweeps
spine crates, canonical homes exempt.**

This document inventories every operation the TypeScript and Rust providers expose or
implement — retrieval side and edit side — compares the two, names each redundancy found
(with evidence), and specifies the consolidation work as reviewable proposals with
acceptance criteria. Proposals are ranked and phased; each can be accepted or rejected
independently.

## Terms

- **Op** — one unit of the operation vocabulary. Three distinct layers get conflated in
  casual use; this spec keeps them apart:
  - **Agent-facing action** — a string the MCP client sends (`"replace_text"`,
    `inspect` mode `"symbol"`). 14 edit actions + 5 inspect modes exist today.
  - **`EditOp` variant** — the internal enum (`ci-core/src/types.rs:162-223`) that
    actions compile into. 15 variants (one, `ReplaceInFile`, is internal-only).
  - **Provider trait method** — what a language provider implements
    (`LanguageProvider`, `ReadIndex`, `GateEngine`).
- **Spine** — the shared implementation in `ci-edit`/`ci-core` that all providers route
  through (`commit_edits`, `reverse_import_map`, etc.). The provider contract §7 forbids
  provider-local reimplementations of spine logic.
- **Composed** — the `ReadIndex` × `GateEngine` glue in `ci-edit/src/lib.rs:1495` that
  implements post-edit read freshness and blast-radius policy once. Today only
  `lang-template` uses it.
- **Fresh overlay** — a provider-held cache of post-edit truth (symbols and/or import
  edges) consulted before the prebuilt index, so reads stay true in-session
  (contract §2).

## 1. Current state — the full op inventory

### 1.1 Agent-facing surface (identical for both languages)

The MCP server advertises exactly two tools (`ci-mcp/src/main.rs:1930`):

- **`apply_edits`** — 14 actions (`ci-edit/src/lib.rs:198-248`): `rename`,
  `replace_text`, `replace_node`, `set_body`, `insert_before`, `insert_in_body`,
  `delete_in_body`, `insert_member`, `add_parameter`, `set_return_type`, `add_symbol`,
  `create_file`, `move_file`, `delete_file`.
- **`inspect`** — 5 modes (`ci-mcp/src/main.rs:809-827`): `search` (semantic
  retrieval), `symbol` (exact/substring name lookup), `file` (anchors + imports),
  `node` (one anchor's source), `map` (architecture overview).

Nothing in this surface is per-language: both providers accept the same actions and
serve the same inspect modes. **There is no TS-op that Rust lacks or vice versa at the
agent-facing layer.** The comparison question therefore reduces to: (a) how each
provider *implements* the shared surface, and (b) whether the shared vocabulary itself
carries internal redundancy.

### 1.2 How each provider implements the surface

| Capability | lang-ts (`TsProvider`) | lang-rust (`RustProvider`) | Shared? |
|---|---|---|---|
| `structure()` | SCIP index + tree-sitter deepen/re-anchor (`ast.rs:18-99`) | live tree-sitter parse (`lib.rs:601-838`) | mechanics differ by design (artifact vs live) |
| `import_graph()` | SCIP + fresh overlay (`lib.rs:405-420`) | optional SCIP cache + `fresh_edges` overlay, syntactic fallback (`lib.rs:293-416`) | overlay logic duplicated — see F2 |
| Sub-node anchors (`:body`/`:params`/`:return`/`:doc`) | `ast.rs:148-212` | `add_fn_subnodes`/`doc_range` (`lib.rs:730-764`) | per-grammar by necessity |
| Cache fingerprint | `fingerprint.rs` (233 lines) | `usegraph.rs:14-92` | **duplicated** — see F1 |
| `outline()` (body elision) | `outline.rs:8-19` | `lib.rs:833` | **duplicated shape** — see F4 |
| `prewarm()` | thread + engine warm (`lib.rs:427-459`, again at `568-594` for `TsTreeGated`) | thread + engine warm (`lib.rs`) | **triplicated pattern** — see F3 |
| `apply_edits()` | `ci_edit::commit_edits` | `ci_edit::commit_edits` | ✅ spine |
| Gate (`GateEngine::diagnostics`) | ts-morph/tsgo sidecar | `cargo check` + `deleted_path_references` gap-fill | per-toolchain by design |
| `rename` / `will_rename` | engine-native (tsserver APIs) | rust-analyzer, `movefix` fallback first | Rust extras are contract-§8 reference models |
| Post-edit read freshness | `fresh` map fed by `file_summaries()`, `lang_fallback` fallback (`lib.rs:496-537`) | `fresh_edges` + `overlay_graph()` (`usegraph.rs:96`) | **duplicated; Composed already owns this** — see F2 |
| Module membership on `create_file` | n/a (imports only) | `declare_module_edit` synthesis | Rust-specific, §8 hook (b) |
| Repo-relative path normalization | `rel()` (`lib.rs:369-376`) | `rel()` (`lib.rs`) | **duplicated** — see F5 |

Neither provider stubs or refuses any op: both advertise `Granularity::Ast`, so all 15
`EditOp` variants and all trait methods are live in both languages. The redundancy is
not "one language has an op the other lacks" — it is *infrastructure implemented twice*
(F1-F5) and *vocabulary overlap within the shared surface* (F6-F9).

## 2. Findings

Each finding states the evidence, then a verdict: **real duplication** (consolidate),
**overlap by design** (keep, document), or **decision needed** (Davi's call).

### F1. Fingerprint machinery implemented twice — real duplication

`lang-ts/src/fingerprint.rs` and `lang-rust/src/usegraph.rs:14-92` both implement:
FNV-1a content hashing, `source_fingerprint()` (gitignore-aware walk → hash map),
`store_fingerprint()` (versioned JSON, atomic write), and drift detection. The genuinely
per-language parts are tiny: the input predicate (which files feed the index) and the
drift-result shape (TS wants a whole-index invalidation reason; Rust wants the per-file
drift list for its overlay). Everything else is copy-shaped. Contract §7's rule — the
byte-vs-char and same-file-batch bugs both came from duplicated logic — applies
directly: a fix to one fingerprint (e.g. the walk-invisible-files handling TS already
has at `fingerprint.rs:86-114`) does not reach the other today.

### F2. In-session read freshness implemented three times — real duplication, migration already settled

The "reads stay true after a committed edit" contract (§2) has three live
implementations:

1. `Composed` in `ci-edit/src/lib.rs:1495-1661` — the blessed one (freshness channel +
   radius policy + anchor consistency), used only by `lang-template`.
2. `TsProvider.fresh` — summaries map populated in `apply_edits` (`lib.rs:496-537`),
   consulted in `structure()`/`import_graph()`.
3. `RustProvider.fresh_edges` + `overlay_graph()` — edge overlay with SCIP-base seeding.

The two-halves provider split (ReadIndex × WriteEngine, Composed glue) is already the
settled architecture with lang-ts and lang-rust migration pending. Finishing that
migration *is* the consolidation for F2 — no new design needed, but the migration must
preserve two provider-specific behaviors Composed must be checked against: TS's
`lang_fallback` re-parse when the engine returns no summaries, and Rust's
SCIP-base + drifted-file seeding on load (`load_scip_base`, `usegraph.rs`).

### F3. `prewarm()` thread-spawn pattern triplicated — real duplication, small

`TsProvider::prewarm` (`lib.rs:427-459`), `TsTreeGated::prewarm` (`lib.rs:581-594`), and
`RustProvider::prewarm` all do: spawn background thread → start engine → issue a warming
call → park it in the shared `Mutex`. Only the engine constructor differs.

### F4. `outline()` duplicated shape — real duplication, trivial

Both are ≤20-line wrappers over `ci_treesitter::body_ranges()` +
`ci_core::elide_bodies()`; only the grammar handle differs.

### F5. `rel()` path normalization duplicated — real duplication, trivial

Both providers privately implement repo-relative POSIX normalization. Same-shaped code;
`ci_core` is the natural home (it already owns `text::byte_offset` for the same
reason).

### F6. `set_body` vs `replace_node` target `:body` — overlap by design, keep

`set_body` is documented sugar (`ci-edit/src/lib.rs:209-211`) and both compile into the
same dispatch (`apply_structural` → `subnode_range("body")`). Single implementation, two
names. Removing the sugar is a *surface* change to the settled two-tool facade — losers
of settled A/Bs are not re-opened without a measured reason. No action beyond a pinned
test that the two spellings stay behavior-identical.

### F7. `replace_text` (node-scoped) vs `ReplaceInFile` (file-scoped) — overlap by design, one hygiene gap

`ReplaceInFile` is not agent-addressable: the MCP dispatcher synthesizes it when name
resolution fails and `oldText` is unique in the file (`ci-mcp/src/main.rs:924-955`), and
`lang-rust` synthesizes it for module-declaration maintenance. The mechanics (unique
substring → VFS write) are shared, and the split keeps the agent vocabulary at one verb.
Hygiene gap: the error string at `ci-edit/src/lib.rs:243-245` enumerates "valid
actions" and is the de-facto vocabulary registry — it is maintained by hand next to the
match. If an action is added without updating it (or vice versa), the reject recipe
lies. See P6.

### F8. `add_parameter`/`set_return_type` vs `replace_text` on `:params`/`:return` — overlap by design, document the split

When the sub-node anchor exists, a return type is reachable two ways (`replace_text`
target `:return` mutates it; `set_return_type` inserts one where none exists — it is
the *upsert-into-absence* verb, anchored to `:params` + `return_delim`). Same for
`add_parameter` (append) vs `replace_text` on `:param.N` (mutate). The intents are
disjoint (create vs mutate) but nothing states that adjacently in the tool description;
an agent that picks the wrong verb gets a resolvable-but-noisy failure. Documentation
fix only — collapsing them into one upsert verb would change reject-recipe contracts
(replies prescribe ready-to-copy fixes; recipes are contract) for no measured win.

### F9. Five removed retrieval tools still accepted by `tools/call` — decision needed

`tools/list` advertises `apply_edits` + `inspect` only, but the `tools/call` dispatcher
(`ci-mcp/src/main.rs:2042-2051`) still accepts `retrieve_context`,
`describe_architecture`, `find_symbols`, `list_anchors`, `read_node` as hidden aliases
for the same server methods. This is the remnant of the removed six-tool surface. It is
*not* code duplication (the handlers are the same functions `inspect` dispatches to),
but it is an unadvertised, untested surface: nothing pins whether these arms should
exist, and a client that discovers them bypasses the settled facade.
**Options:** (a) delete the arms — the facade decision says two tools IS the surface;
(b) keep them as deliberate back-compat with a test naming them as such.
Recommendation: (a), since the six-tool surface was removed by a settled A/B and stale
*binary* registrations (the known back-compat concern) are unaffected by tool-name
arms in the new binary.

### F10. Language knowledge inside the shared spine — real (inverse) duplication, two sites

The contract bans providers reimplementing spine logic; these are the inverse — the
spine hard-coding language facts that belong to providers:

- `return_delim()` (`ci-edit/src/lib.rs:899-904`): chooses `->` vs `:` by file-extension
  suffix check. Adding a language with a different return syntax means editing `ci-edit`.
- `is_import_line_for()` (`ci-edit/src/lib.rs:968`): syntactic import-line detection used
  by the `delete_file` safety check, parallel to the real per-provider import resolvers
  every provider already has (and overlapping the §8 deleted-reference model, whose
  Rust reference implementation `deleted_path_references` does the resolver-backed
  version of the same job).

### F11. Capabilities scattered across files instead of organized around their wrapper — real, structural

The intended architecture is: **the core crates hold every generic capability, organized
and accessible (one capability, one module); language crates only extend the core** with
their grammar hooks, toolchain probes, and engine adapters. Today several files violate
that by accretion:

- `lang-rust/src/lib.rs` (1546 lines) mixes five capabilities: the provider impl, the
  `GateEngine` impl + gate helpers (`cargo_check_diags`, `deleted_path_references`),
  structure extraction (`collect_items` family), module/graph resolution (`mod_decls`,
  `resolve_mod`, `rust_files` — which duplicate scope logic with `usegraph.rs`, noted
  in the lang-rust inventory), and `outline()`.
- `lang-rust/src/usegraph.rs` holds the *fingerprint* machinery — cache-freshness logic
  living in a file named for use-path resolution (P1 removes it; the residue should be
  a coherent graph module).
- `lang-ts/src/lib.rs` (1061 lines) holds two providers (`TsProvider` + the
  `TsTreeGated` ablation), engine selection, indexing orchestration, and NPX locking in
  one file.
- `ci-edit/src/lib.rs` (~1700 lines) holds the action→`EditOp` mapping, all fifteen
  apply handlers, the gate loop, `Composed`, and the `GateEngine` trait in one file —
  the spine is the most-read code in the repo and the least navigable.

Verdict: reorganize as **pure module moves with zero public-API change** (re-exports
keep every existing path compiling). This is P11.

### Explicitly examined and cleared

- `insert_before` vs `add_symbol`: complementary (append-at-end is inexpressible with a
  later-anchor verb); the doc comment at `ci-edit/src/lib.rs:229-231` already says so.
- `delete_in_body` vs `replace_text` old→`""`: statement semantics (whole-line handling,
  uniqueness check) are distinct from substring replacement; both are load-bearing in
  reject recipes.
- `inspect(search)` vs `inspect(symbol)`: semantic fusion vs exact name lookup — the
  fusion *includes* a symbol-name signal, but the exact mode is the cheap, deterministic
  path; complementary.
- Rust-only `movefix` / `declare_module_edit` / `deleted_path_references`: not
  redundancy — these are the §8 reference models, extraction deliberately deferred to
  the next language landing. This spec does not re-decide §8.
- TS `ast.rs` re-anchor/deepen vs Rust `collect_items`: both produce the same `Node`
  contract from different substrates (SCIP artifact vs live parse); per-grammar code is
  the part that is *supposed* to be per-language.

## 3. Proposals

Ordered by phase. Phase 1 items are pure internal dedup (no surface change, no bench
run needed — conformance green is the bar). Phase 2 is the settled migration. Phase 3
items change observable surface or spine structure and need sign-off individually.

### Phase 1 — extract duplicated infrastructure (F1, F3, F4, F5)

**P1. Shared fingerprint module** (addresses F1)
- Add `ci_core::fingerprint` (module, not a new crate): `fnv1a`, `Fingerprint` type,
  `source_fingerprint(root, is_input: impl Fn(&Path) -> bool)`,
  `store_fingerprint`/`load_fingerprint` (versioned, atomic), and BOTH drift shapes:
  `drift_reason() -> Option<String>` and `drifted_files() -> Option<Vec<String>>` over
  the same diff walk.
- Providers keep only: their `is_input` predicate, their fingerprint path, their
  version constant, and TS's augmented-files re-hash hook (walk-invisible files,
  `fingerprint.rs:86-114`) — passed in as an extra file list, not reimplemented.
- Acceptance: both providers' existing fingerprint unit tests pass unchanged against
  the shared module (they move with it, parameterized per provider); byte-identical
  fingerprint files before/after (same hash, same JSON shape) so no reindex storm on
  upgrade; `cargo test -p ci-conformance` green.
- Risk: low. The subtle behaviors (augmented files, version-bump-reads-as-drifted) are
  pinned by existing tests that come along.

**P2. `prewarm` helper** (addresses F3)
- One spine helper (in `ci-edit`, next to `GateEngine`):
  `spawn_prewarm(slot: Arc<Mutex<Option<E>>>, make: impl FnOnce() -> Option<E> + Send)`
  that owns the thread/lock/already-warm discipline; providers pass their engine
  constructor + warming call.
- Acceptance: three call sites collapse; prewarm-vs-cold-start race behavior unchanged
  (pin with a test that `apply_edits` during an in-flight prewarm waits, not
  double-starts — today's `is_some()` guard discipline).

**P3. `outline` helper** (addresses F4)
- `ci_treesitter::outline(language, content) -> String` wrapping
  `body_ranges` + `elide_bodies`; providers become one-line calls.
- Acceptance: identical output on both providers' existing outline tests.

**P4. `rel` path helper** (addresses F5)
- `ci_core::rel_path(root, path) -> String` (posix, repo-relative, absolute-or-relative
  input); both providers' private `rel()` delete.
- Acceptance: property test with mixed-separator/absolute/relative inputs matching the
  union of today's two behaviors (verify they don't disagree first — if they do, that
  is a latent bug to surface in review, not silently paper over).

**P5. Make the §7 audit executable** (guards all of the above)
- The contract says "the audit greps for reimplementations" but no such check exists in
  `ci-conformance`. Add a test that greps `crates/langs/*/src` for the signatures this
  spec just consolidated (`fn fnv1a`, `fn source_fingerprint`, `fn rel(`,
  body-elision call shape, thread-spawn prewarm shape) and fails with the clause name.
  New languages then can't re-introduce the duplication this spec removes.

### Phase 2 — finish the two-halves migration (F2)

**P6. Migrate lang-ts and lang-rust onto ReadIndex × WriteEngine + Composed**
- Already the settled architecture (lang-template is the template). The op-redundancy
  payoff: deletes `TsProvider.fresh` + its populate logic (`lib.rs:496-537`) and
  `RustProvider.fresh_edges`/`overlay_graph`, and removes the per-provider
  radius-policy knowledge (Composed reads `semantic_edges()` — which also absorbs
  `TsTreeGated`'s hand-rolled transitive-radius call at `lib.rs:609`).
- Composed capabilities to verify against provider-specific behavior before cutover
  (these are the migration's real risks):
  1. TS: engine `file_summaries()` returns `None` on LSP engines → Composed's
     `live_summarizer` fallback must reproduce today's `lang_fallback` re-parse.
  2. Rust: `load_scip_base` seeds `fresh_edges` from `drifted_files()` at open —
     Composed needs an equivalent "stale-at-load overlay" channel, or Rust's ReadIndex
     half does the seeding internally before Composed sees it (preferred: keep it in
     the read half; it is read-side truth, not glue).
  3. Rust: `edges_from_disk` post-commit edge refresh vs Composed's summary-driven
     refresh — confirm the ImportGraph overlay Composed builds matches `overlay_graph`
     semantics (replace / blank / delete entries).
- Acceptance: full conformance battery + both providers' `#[ignore]` e2e suites green
  (notably `committed_edit_refreshes_reads_in_session`, the R2 move suite, and the
  delete-refusal recipe e2e); one bench spot-check run (rust + ts arms) with no cost
  regression, since read-freshness is on the hot path of every post-edit turn.
- Sequencing: migrate lang-rust first (its read half is live tree-sitter — closer to
  lang-template), then lang-ts (artifact-backed read half exercises the harder seams:
  fresh-overlay + re-anchor).

### Phase 3 — surface & spine hygiene (F7, F8, F9, F10) — each needs individual sign-off

**P7. Delete the hidden legacy tool arms** (F9, recommendation (a))
- Remove the five unadvertised `tools/call` arms; add a conformance/MCP test asserting
  the accepted-call set equals the advertised `tools/list` set (this test is the real
  deliverable — it pins the facade decision permanently).
- If (b) is chosen instead: keep the arms, add the same test with the aliases named as
  deliberate back-compat, and a removal date.

**P8. Move `return_delim` into `LangSpec`** (F10, first site)
- The registry's `LangSpec` (extensions, ignore dirs) grows a `return_type_delimiter`
  field; `ci-edit` reads it instead of suffix-matching. Mechanical; makes
  `set_return_type` correct-by-construction for the next language instead of
  edit-the-spine.

**P9. Route the `delete_file` import check through provider resolvers** (F10, second site)
- Replace `is_import_line_for()`'s generic line heuristic with the §8 deleted-reference
  model: resolve surviving files' references via the provider's import resolver (every
  provider has one; Rust's `deleted_path_references` is the reference semantics).
- This is deliberately staged *behind* the §8 extraction trigger: do it when the next
  language lands and §8 gets extracted anyway — doing it now would build the generic
  form with only one validating consumer, which §8 explicitly forbids. Listed here so
  the overlap is on record, not forgotten.

**P10. Disambiguate create-vs-mutate verbs in the tool description** (F8)
- One sentence each on `add_parameter`/`set_return_type` ("adds where none exists; to
  change an existing one, `replace_text` on `:param.N`/`:return`") — description-only
  change, but tool-description text is measured surface (it changed T5/T8 outcomes), so
  it rides along with the next bench-validated description change rather than shipping
  blind. Generic phrasing only — no bench-fixture names.

**P11. Organize capabilities around their wrappers** (addresses F11)
- Pure module moves, zero public-API change (`pub use` re-exports preserve every
  existing import path). Three sub-tasks, executed after the Phase-1 extracts (so the
  moves operate on the already-deduplicated residue):
  1. `ci-edit`: split `lib.rs` into `actions.rs` (action→`EditOp` mapping),
     `apply.rs` (per-op handlers), `composed.rs` (`Composed`); the `GateEngine` trait +
     `commit_edits` spine stay in `lib.rs`.
  2. `lang-rust`: `structure.rs` (item/sub-node extraction), `graph.rs` (module + use
     resolution, absorbing `usegraph.rs`'s post-P1 residue and the `mod_decls`/
     `resolve_mod`/`rust_files` trio from `lib.rs`), `gate.rs` (`cargo_check_diags`,
     `deleted_path_references`, `RustEngine`); provider impl stays in `lib.rs`.
  3. `lang-ts`: `ablation.rs` (`TsTreeGated`), `engine.rs` (engine selection + NPX
     locking); `TsProvider` stays in `lib.rs`.
- Acceptance: `cargo build --workspace` + full non-ignored test suite green with **no
  test file edits** (proves the API held); each capability findable by module name.

### Non-goals (explicit)

- No change to the two-tool facade or the 14-action vocabulary (settled A/B; F6/F7/F8
  overlaps are documented-by-design, not removed).
- No extraction of `movefix`/`deleted_path_references` ahead of the §8 trigger.
- No new crate for fingerprinting (module in `ci_core` suffices; a crate is ceremony).
- No touching the Node prototype.

## 4. Open questions for review

1. **F9/P7**: delete the hidden legacy arms, or keep as named back-compat? (Rec: delete.)
2. **P6 sequencing**: agree rust-first? And is a bench spot-check (rust + ts arms, one
   run each) the right regression bar for the migration, or full suite?
3. **P4**: if the two `rel()` implementations turn out to disagree on some input class,
   surface-and-fix or preserve-per-provider? (Rec: surface — a disagreement is a bug.)
4. **P10**: fold into the next bench-validated description change, or is doc-only churn
   acceptable to ship immediately?

## 5. Execution order & effort (once approved)

| Step | Items | Size | Gate |
|---|---|---|---|
| 1 | P1, P3, P4 (mechanical extracts) | S-M | conformance + provider unit tests |
| 2 | P2, P5 (prewarm helper, executable audit) | S | conformance |
| 3 | P6 lang-rust migration | M | conformance + rust e2e + bench spot-check |
| 4 | P6 lang-ts migration | M-L | conformance + ts e2e + bench spot-check |
| 5 | P7 (pending decision), P8 | S | MCP surface test |
| 6 | P9, P10 | — | deferred to their triggers (§8 / next bench run) |
