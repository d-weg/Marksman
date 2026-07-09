# Test-surface consolidation — spec for review

**Status: DRAFT — nothing in this document is implemented. It is a design spec for review.**

This document does for the test suite what `op-surface-consolidation-spec.md` did for the
op surface: a full inventory (three parallel sweeps: unit tests, integration/e2e tests,
test infrastructure), each redundancy named with evidence and a verdict, and the work
specified as independently acceptable proposals. One headline difference from the
function pass, stated up front because it shapes everything below: **the test suite is
already largely sound** — the consolidation is small, and the bigger findings are
*gaps* (contract clauses without executable enforcement), not duplication.

## Terms

- **Tier** — where a test runs: **fast unit** (in-crate `#[cfg(test)]`, no external
  tools), **real-tool e2e** (`#[ignore]`, needs rust-analyzer/npx/etc., run with
  `-- --ignored`), **battery** (the shared conformance suite in `ci-conformance`,
  parameterized over provider instances).
- **Battery fixture** — a `ReadFixture`/`EditFixture` (`ci-conformance/src/lib.rs:23-57`)
  applied to a provider by `run_read_battery`/`run_edit_battery`. The provider contract
  says the battery IS the enforcement; per-provider tests exist for what the battery
  cannot express (real toolchains, language-specific op synthesis, §8 reference models).
- **Contract pin** — a test named by `docs/provider-contract.md` as the executable form
  of a clause (e.g. `committed_edit_refreshes_reads_in_session` for §2).

## 1. Current state

**264 tests: 215 fast unit + 49 real-tool `#[ignore]`**, across 39 `#[cfg(test)]`
modules and 2 `tests/` dirs (`ci-conformance` battery; `lang-rust` sidecar). Convention
health is good and uniform: tests live in-crate per contract §7, `tempfile` is universal
where file I/O exists (17/21 crates, zero hand-rolled temp dirs), `#[ignore]` is applied
per-test and consistently means "needs a real toolchain".

Distribution of the e2e weight: `lang-rust` 31 ignored (16 gate/move/delete pins + the
sidecar suite), `lang-ts` 14, spine (`ci-edit`) 4, conformance real-tool instances 3.

Shared test infrastructure is nearly nonexistent — and mostly *rightly* so:
`ci-conformance` exposes the fixture types publicly but is its own only consumer (by
design: providers implement the trait, the battery tests them); `ci-edit` has a
`pub(crate)` `testutil` with one Node builder; every crate writes its own inline
fixtures. The dev-dependency graph (conformance dev-depends on all four lang crates;
almost nothing else cross-depends) confirms there is no hidden sharing to formalize.

## 2. Findings

Verdicts as in the op-surface spec: **real duplication** (consolidate), **overlap by
design** (keep, document), **gap** (add enforcement), **decision needed**.

### F1. Five GateEngine/ReadIndex mocks hand-rolled inside ci-edit — real duplication

The one genuine helper-duplication cluster, and it is entirely within one crate:
`lib.rs`'s test mod defines `NoopEngine` (:1053), `FanoutEngine` (:1124),
`DupDiagEngine` (:1149); `composed.rs`'s test mod separately defines `StubReader`
(:240), `SummaryEngine` (:267), plus `summary()`/`base_graph()` builders. Each is a
bespoke `GateEngine`/`ReadIndex` impl differing only in which method is scripted. Every
future spine test will need one of these; today it would grow a sixth mock. The existing
`testutil` module (`lib.rs:849`, just `fn_node`) is the natural home.

### F2. One fully redundant e2e: `indexes_real_ts_project_via_scip` — real duplication (verify before deleting)

`lang-ts/src/lib.rs:459` and the battery's `conformance_ts_scip`
(`ci-conformance/tests/conformance.rs:360`) run the same provider through the same
indexer (`TsProvider::index` → scip-typescript) with equivalent fixtures and equivalent
assertions (structure ids + import edge). The inventory found no unique assertion in the
in-crate copy. This is exactly the pattern the contract says should not exist ("a new
language ships by adding its fixtures there"). Verdict: delete the in-crate test with a
pointer comment — **after** a verification pass confirms full subsumption; any assertion
found to be unique moves INTO the battery fixture instead of dying with the test.

### F3. Everything else that looks duplicated is overlap by design — keep, document

Each of these was individually examined and earns its place; listed so the next audit
doesn't re-litigate them:

- `sidecar_round_trips_read_path` vs `conformance_rust_reads`: same fixture shape, but
  the sidecar test's subject is the **wire protocol** (sub-nodes surviving protobuf) —
  the battery never crosses process boundaries.
- `treesitter_gated_gates_and_renames` vs `conformance_ts_fallback`: semantic (ts-morph)
  gate vs syntactic fallback gate — different gate *implementations*, both contractual.
- The two outline tests (`ci-core/outline.rs:38` vs `lang-ts/outline.rs:19`): algorithm
  in isolation vs tree-sitter integration.
- `leaf()` (ci-core) vs `fn_node()` (ci-edit): generic tree-walk fixture vs
  edit-anchoring fixture; different invariants under test.
- The lang-rust §8 suite (`committed_move_must_leave_a_compiling_crate` and friends) and
  lang-ts's §2/§3 pins (`committed_edit_refreshes_reads_in_session`, the barrel and
  monorepo tests): these are contract pins named by the contract itself, not battery
  duplicates — the battery has no move fixtures and no post-edit-freshness probe.
- Unit-test behavior overlap across crates: **none found.** Every consolidated shared
  function (fingerprint, `rel_path`, `byte_offset`, graph inversion, config) is tested
  exactly once, at its source. The function-consolidation batch left no test residue.

### F4. Contract clauses without executable enforcement — gaps (the real work)

The inventory mapped every test to the contract clause it enforces; four clauses came
back short. In the spirit of the §7 audit (P5): a clause the suite doesn't execute is a
clause the next change can silently break.

- **G1 (§5, baseline-diff)** — "pre-existing breakage never blocks an unrelated edit" is
  the gate's defining property, and NO test anywhere exercises it: the battery only
  checks introduced-breakage-rejects. A regression here (gate rejecting everything in a
  repo with any pre-existing error) would ship green today.
- **G2 (§3, transitive radius for syntactic tiers)** — the clause exists because of a
  measured false-clean (bench T9), it is pinned for TS-with-scip via the barrel e2e, but
  the battery never verifies an UNGATED tier's edit gate actually receives the
  transitive closure. The fallback tier could regress to one-hop unnoticed.
- **G3 (§6, missing toolchain → disabled-with-reason)** — partially covered:
  `ci-build/registry.rs:336` pins the registry-level behavior
  (`missing_toolchain_is_disabled_with_reason_not_failed`). Unverified: that each gated
  provider's own `toolchain()` probe feeds it honestly (e.g. `CI_RUST_ANALYZER` pointing
  at a nonexistent binary). Verify what exists before writing anything.
- **G4 (§2, Rust in-session freshness)** — flagged as a gap by one sweep, but
  `scip_graph_stays_fresh_after_edits` (`lang-rust/src/lib.rs:789`) plus the post-P6
  Composed unit pins (`composed.rs:316,348,400`) appear to cover it. Verify-first: if
  the e2e pins both structure AND graph freshness post-commit, reclassify as covered and
  close; only extend if the structure half is genuinely unpinned.

### F5. Stale test-count claims in docs — trivial

`docs/architecture.md` says "~150 unit tests"; the tree holds 215 + 49 ignored. Numbers
in docs rot fast — prefer stating the *shape* (fast unit + `#[ignore]` e2e + battery)
and the command, not a count.

### Explicitly examined and cleared

- Temp-dir/fixture scaffolding: uniform `tempfile` idiom everywhere; centralizing would
  over-abstract (a `tiny_crate()` here, a `write()` there are context-shaped, ~5 lines
  each). **No shared test-util crate** — same rule as the §8 extraction: don't extract
  speculatively; the cross-crate second consumer doesn't exist.
- Bench fixtures (`scripts/agent-bench/fixture-*`): different purpose, out of scope, no
  test duplicates them.
- `load_timing` (`ci-index/src/store.rs:190`): an `#[ignore]` perf probe, not a
  correctness test; fine as-is.
- tests-dir usage (2 crates): both justified (battery; external sidecar process).

## 3. Proposals

### Phase 1 — consolidate (small, mechanical)

**T1. One `testutil` module for ci-edit's mocks** (F1)
- Move `NoopEngine`, `FanoutEngine`, `DupDiagEngine`, `StubReader`, `SummaryEngine`,
  `summary()`, `base_graph()`, `fn_node()` into the existing `testutil` module
  (`cfg(test)`, `pub(crate)` — in-crate only, NOT a public API and NOT a new crate);
  `lib.rs`/`apply.rs`/`composed.rs` test mods import from it.
- Acceptance: zero test-behavior change (same test names, same assertions, counts
  identical); `cargo test -p ci-edit` green.

**T2. Retire the redundant TS scip e2e** (F2)
- Verification step first: diff the assertions of `indexes_real_ts_project_via_scip`
  against `conformance_ts_scip`; port any unique assertion into the battery fixture;
  then delete the in-crate test, leaving a one-line pointer comment naming the battery
  test as the canonical home.
- Acceptance: `cargo test -p ci-conformance -- --ignored` green with assertions ≥ the
  union of both tests' checks; lang-ts ignored suite green.

### Phase 2 — close the contract gaps (the value)

**T3. Baseline-diff battery case** (G1) — extend `run_edit_battery` (or add a sibling
`run_baseline_battery`) with: fixture containing one PRE-BROKEN file; a clean edit to an
unrelated file must COMMIT; an edit that introduces a new error must still REJECT — and
the pre-existing diagnostic must never appear in the reject reply. Run it over the
ungated instances + the template's mock gate (which can script "pre-existing" cheaply).
Real-tool instances get it via one new `#[ignore]` case each for lang-rust/lang-ts only
if the mock-tier version can't express something real (decide during implementation,
justify in the PR).

**T4. Transitive-radius battery case** (G2) — fixture: `a → barrel → c` re-export chain
on a syntactic-tier provider; an edit to `c` that breaks `a` must reject even though
`a` is two hops away. This is the battery-expressible form of the TS barrel e2e, run
over every `semantic_edges() == false` instance.

**T5. Toolchain-honesty verification** (G3) — verify-first task: map what
`registry.rs:336` already covers; add only the missing per-provider probe test(s)
(e.g. env-var-pointed-at-nonexistent-binary → `ProviderBuild::Unavailable` with the
install hint, never a silent fallback tier). May conclude "already covered" with
evidence — that is a valid outcome.

**T6. Rust freshness reclassification** (G4) — verify-first task: read
`scip_graph_stays_fresh_after_edits` + the Composed pins; produce either a written
"covered, here's the mapping" note in the spec, or the one missing assertion added to
the existing e2e (not a new test).

### Phase 3 — document the boundary (so the audit doesn't re-litigate)

**T7. Test-tier boundary note** — a short section in `docs/provider-contract.md` (§7 or
a new "testing" subsection) stating what each tier owns: battery = contract clauses over
fixtures; provider unit = language extraction + hooks; provider `#[ignore]` = real-tool
proof + contract pins named by clause; spine unit = op semantics + glue channels.
Providers must NOT re-test battery-covered behavior in-crate (F2 is the precedent), and
module docs on the two provider test mods say so. Plus the F5 doc-count fix
(shape + command instead of a number).

### Non-goals (explicit)

- No shared test-util crate, no cross-crate helper exports (F3/cleared: the second
  consumer doesn't exist; ci-edit's consolidation is in-crate).
- No thinning of the real-tool e2e tier: 49 ignored tests is the cost of honest gates,
  and every one maps to a clause or a shipped bug.
- No battery API break: T3/T4 extend fixtures/batteries additively; existing fixture
  definitions keep compiling.
- Bench fixtures and the Node prototype untouched.

## 4. Open questions for review

1. **T2**: agree the in-crate scip e2e dies (post-verification), or keep both on
   belt-and-suspenders grounds? (Rec: delete — the contract's own rule.)
2. **T3 shape**: extend `run_edit_battery` vs a new `run_baseline_battery`? (Rec: new
   sibling — keeps existing fixtures untouched and the clause independently runnable.)
3. **T3/T4 real-tool instances**: mock-tier only, or also one `#[ignore]` case per gated
   provider? (Rec: decide during implementation; mock-tier is the floor.)
4. Execution as before: orchestrated workflow, item-per-item, adversarial verify,
   uncommitted for review?

## 5. Execution order & effort (once approved)

| Step | Items | Size | Gate |
|---|---|---|---|
| 1 | T1 (mock consolidation) | S | cargo test -p ci-edit |
| 2 | T2 (redundant e2e, verify-then-delete) | S | conformance + lang-ts ignored suites |
| 3 | T5, T6 (verify-first gap checks) | S | evidence notes or minimal test additions |
| 4 | T3 (baseline-diff battery) | M | conformance suite incl. new battery |
| 5 | T4 (transitive-radius battery) | M | conformance suite |
| 6 | T7 (boundary docs + count fix) | S | doc review |
