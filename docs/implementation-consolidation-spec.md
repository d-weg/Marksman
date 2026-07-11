# Implementation consolidation — spec for review

**Status: EXECUTED (2026-07-11, branch `consistency-audit` off `container-gate`) — all of
P1–P13 landed, one proposal per commit, verified green.** The record:

- **Soundness (P1–P4):** `run_gate_capped` + `silent_tool_failure_diag` + `Error::GateTimeout`
  in ci-core; rust verdict timed/capped with timeout PROPAGATING past the ra fallback
  (`Sandbox::output` deleted from the trait — an untimed gate is now unrepresentable, pinned
  by a stub-sandbox unit test); java sidecar `recv_timeout` + kill-on-hang, mvn/gradle capped;
  swift `describe`/`prime_index` through the sandbox, **G4 fail-closed** (a describe failure on
  a real package refuses the edit; not cached, so a fixed toolchain recovers).
- **Dedup (P5–P8):** `ci_core::discover_tool` (4 lookups); `ci_edit::LazyLsp` (the java/php/swift
  LSP-rewrite half); `ci_edit::moves::dotted` — the generic dotted-name move engine, java/php as
  `DottedSyntax`+`DottedLang` instances with every pinned span test passing unchanged; new
  `ci-providers` crate hosting the one `make_provider` (the CLI gains the php/swift gated arms —
  the F8 drift, closed). Recorded LEAVEs stand (provider lib.rs delegation, sidecar bins,
  CI_TIMING).
- **Consistency (P9–P12):** serverInfo `"marksman"`; all agent-facing text rewritten in
  `inspect`-mode vocabulary; `MOVE_COVERAGE` for java/php/swift wired + pin test extended to
  five languages; `CI_PARITY_REPO` override. **Bonus:** clippy is now 0-warning across
  `--all-targets` (the tree never was; all 31 pre-existing findings fixed — type aliases,
  `while let`, `repeat_n`, `is_none_or`, a dead recursion param).
- **Docs (P13):** architecture.md crate table += the three rollout crates + ci-providers, Java
  out of the ungated examples; provider-contract.md Tiers current, §8 present-tense, **new §9**
  (sandbox seam + gate-timeout semantics); roadmap.md Batch 8 records the rollout.

**Verification:** `cargo test --workspace` 257/257 · `cargo clippy --workspace --all-targets`
0 warnings · conformance real-tool battery 6/6 (java/php×2/swift/ts-scip/ts-lsp-sweep) ·
per-crate `#[ignore]` tiers: rust 18/18, java 10/10, php 6/6, swift 8/8 — **including all four
`oci_*_without_host_tools` container e2es** (docker + images present on this host). Not run
here (tools absent, tests self-skip loudly): mvn/gradle classpath derivation, host-jdtls and
host-phpactor rename e2es — the container e2es exercise those renames in-image. The one pre-existing
failure found during verification (failed identically at base `5ba01dc`) was ROOT-CAUSED and
FIXED in the follow-up commit: the tsls-gate e2es false-cleaned because (a) npm's `typescript`
latest is now the 7.x Go line, which ships no tsserver, so tsls errors at `initialize` and
exits, and (b) `LspClient::start_in` never checked the initialize response for an error — the
dead server's silence read as clean through the push-diagnostics path. Fixed both ways: a
failed initialize is now a loud `Err` in ci-lsp (the soundness half — production was already
version-pinned and unaffected), and the two test launchers pin the same tsls/typescript pair
as lang-ts's production tier. All three e2es green after the fix; workspace 257/257, clippy 0.

The original audit follows unchanged.

---

This document does for the implementation what `op-surface-consolidation-spec.md` did for the op
surface and `test-surface-consolidation-spec.md` did for the tests: a full audit (three parallel
sweeps: spine crates, language providers, cross-cutting surfaces — specs/tests/MCP), each finding
named with evidence and a verdict, and the work specified as independently acceptable proposals.
The headline, stated up front: **the architecture is sound** — the crate graph is a clean DAG
rooted at `ci-core`, there is zero dead code and not a single TODO in the tree, and the LSP
quiescence logic every provider depends on lives in exactly one place. But the java/php/swift
rollout left **two real soundness gaps in the gate tier** (an untimed Rust gate on a
single-threaded server; a Swift honesty check that fails open in containers) and **~700 lines of
copy-paste** that the contract's own "extract on the second consumer" rule says should have been
hoisted. Those, plus the foundational docs trailing the rollout, are this spec.

## Terms

- **Gate verdict** — the compiler pass that decides whether an edit batch commits
  (`cargo check` / javac sidecar / phpstan / `swift build`). Contract §5. A verdict tool that
  hangs or dies must **refuse** the edit — never pass it, never swap in a weaker engine
  (CONTRIBUTING: "never silently degrade a gate").
- **Rollout provider** — lang-java / lang-php / lang-swift: `Composed<FallbackProvider>` reads ×
  a real-compiler `GateEngine`, built by copying `lang-template` per the provider rollout ladder.
- **Sandbox seam** — `ci_core::resolve_sandbox(root, image)` + `ci_core::tool_command`: every
  gate/rename subprocess routes through a `Sandbox` (host or OCI container), per
  `container-gate-spec.md`.
- **Fail-open / fail-closed** — what a safety check does when its own machinery fails: skip
  itself (open) or refuse the operation (closed).
- **§N** — clause N of `docs/provider-contract.md`.

## 1. Current state

17 spine crates + 7 lang crates; the dependency graph is a clean DAG (no cycles, no back-edges).
Largest files: `ci-mcp/src/main.rs` 2,376 lines (the last monolith — split already specced as
file-surface P1), `ci-edit/src/lib.rs` 1,403, `lang-rust/src/lib.rs` 1,195. Error handling is a
`thiserror` enum in the libraries (`ci_core::Error`), `Diag` conventions are uniform across all
four gates, env vars are uniformly `CI_`-prefixed (~30 read sites), logging is uniformly
`eprintln!` with bracketed prefixes. The conformance battery drives all seven provider instances
green. Real-tool e2e is `#[ignore]`, uniformly.

The rollout providers are structurally faithful to `lang-template` — same wrapper, same 6-method
delegation, same engine-module trio (`gate` + LSP launcher + `movefix`). The copy-paste debt is
concentrated in exactly the places the template couldn't reach: the movefix hooks, the
LSP-rewrite half of each engine, tool discovery, and the two binaries' provider assembly.

## 2. Findings

Verdicts as in the op-surface spec, plus one this audit needs: **soundness flaw** (a gate can
lie — fix first), **real duplication** (consolidate), **overlap by design** (keep, document),
**gap** (add what's missing), **decision needed** (Davi's call).

### F1. The Rust gate verdict is uncapped and untimed — soundness flaw

`lang-rust/src/gate.rs:78` runs `cargo check` via `Sandbox::output()`, whose own doc
(`ci-core/src/sandbox.rs:37-40`) says UNCAPPED/UNTIMED — while php (`gate.rs:131`) and swift
(`gate.rs:56`) bound their verdicts with `run_capped(gate_timeout())` and REJECT on timeout
(rollout-audit B3/B4). The highest-traffic gate is the unprotected one, and the MCP server loop
(`ci-mcp/src/main.rs:2087`) is single-threaded: a hung `cargo check` freezes Marksman whole.
Compounding trap: `RustEngine::diagnostics` (`gate.rs:139-149`) treats *any* error as "cargo
unavailable" and falls back to rust-analyzer diagnostics — so a naive timeout-as-`Err` fix would
silently swap the verdict engine, the exact degrade the house rule forbids. The fix must make
timeout a *distinguished* error that propagates (edit refused, disk untouched).
Truncation-soundness (checked): a capped stream can only DROP diagnostics on an already-failing
exit code; with the reject-on-failed-tool invariant (F4) every drop path still rejects. No
false-clean is reachable.

### F2. The Java gate has no deadline anywhere — soundness flaw

The sidecar round-trip blocks on `read_line` with no timeout (`lang-java/src/gate.rs:82-84`); a
wedged javac sidecar hangs the edit (and the server, per F1's single-thread note).
`maven_classpath` (`:190-194`, `.status()`) and `gradle_classpath` (`:212-220`, `.output()`) can
hang on network with no deadline — though these are classpath *derivation*, not the verdict:
their documented failure mode (`:149-176`) is an honest degrade (warn + dependency types become
baseline errors), so a timeout there may map to `None`, unlike the verdict path.

### F3. Swift's G4 untargeted-file check fails open under `CI_SANDBOX=oci` — soundness flaw

`describe_target_dirs` (`lang-swift/src/gate.rs:286-311`) and `prime_index` (`:216-227`) call raw
`Command::new("swift")` — no sandbox, always host swift. In the container configuration (the one
case where host swift is typically absent) describe fails, `target_dirs` caches `None`, and
`reject_untargeted` (`:258`, the G4 honesty check from the rollout audit) silently skips: an edit
to a file `swift build` never compiles can sail through ungated. The struct doc (`:237`) even
records the fail-open. House rule says fail-closed; nothing in the test suite pins the fail-open
(verified). `Ok(None)` stays legitimate only for "no Package.swift at all".

### F4. The reject-on-failed-tool invariant is hand-kept in three copies — real duplication (soundness-adjacent)

"Non-zero exit + zero parsed diagnostics ⇒ the tool died before reporting; synthesize the one
`Diag` that makes the spine REJECT instead of reading silence as clean" — implemented at rust
`gate.rs:112-116`, php `gate.rs:149-157`, swift `gate.rs:80-87`, with comments literally
cross-referencing each other. Twelve lines each, but it is the single rule standing between a
crashed compiler and a false-clean commit; three hand-synchronized copies is two too many. Only
the message extraction (stderr-first-line vs `contains("error:")`) is legitimately per-language.

### F5. lang-java and lang-php movefix are ~75% the same file — real duplication (~200–250 lines)

`lang-java/src/movefix.rs` (353 lines) and `lang-php/src/movefix.rs` (382) share structure
member-for-member: token-bounded span finder (`fqn_spans`/`fqcn_spans`), dotted-run scanner
(`dotted_runs`/`backslash_runs`), `ref_occurrences` (identical control flow: mask
strings/comments, scan imports, longest-prefix-resolve code mentions), `membership_edits`
(identical: same-scope ⇒ `Some(vec![])`, `ReplaceLine`/drop, `InsertAt` when none). What varies
is a config's worth of scalars (separator `.` vs `\`, `import` vs `use`, trailing-separator
semantics, alias/group-import rules) plus five per-language hooks (resolver, file→ref, decl
scan/render, insert anchor, masking). §8 hoisted the *engine* (`ci_edit::moves`) with lang-rust
as reference — but the second and third consumers' *hooks* were copied instead of factored. This
is the contract's own "extract as the next language lands" rule, half-honored. Swift's movefix is
the intentional degenerate no-op; rust's is genuinely mod-decl-specific — neither is part of the
twin.

### F6. The LSP-rewrite half of the java/php engines is byte-identical — real duplication (~70–90 lines)

Lazily-started-LSP accessor (java `gate.rs:246-252` = php `:226-232` = swift `:245-250`),
`will_rename` engine-native-first-then-movefix (java `:274-292` = php `:254-268` modulo comment),
`sync_disk`/`fs_events` delegate-or-`Ok(())` (three identical copies). Swift keeps one honest
specialization (`prime_index` + `_pollIndex` inside `rename`); rust's LspClient is non-lazy and
stays out.

### F7. Tool-binary discovery re-implemented six times — real duplication (~80–90 lines)

"`CI_<TOOL>` env → PATH scan → hardcoded fallback dirs → `Option<PathBuf>`": `jdtls.rs:32-51`,
`phpactor.rs:22-43`, `sourcekit.rs:29-52`, php `gate.rs:25-45`, and a *private* `find_on_path`
already sitting in `ci-core/src/sandbox.rs:114-122` that nobody could call. Note: rust
(`lib.rs:66-76`) and ts (`engine.rs:116-124`) look similar but are semantically different — they
trust the env var *unconditionally* (explicitly-set-but-wrong fails loudly later instead of
silently falling through). That difference is a behavior decision, not dedup; they stay.

### F8. `make_provider` copy-pasted across the two binaries — and already drifted — real duplication (~150 lines)

`ci-cli/src/main.rs:50-155` vs `ci-mcp/src/main.rs:30-181`: same sidecar preamble, same per-arm
`describe_missing`/`gate_missing`/`ProviderBuild` logic. The mcp copy is a superset — the cli
**silently lacks the php/swift gated arms** (those repos fall to ungated fallback in the CLI) and
the missing-optional-tool warnings. This is the textbook cost of the copy: the two binaries no
longer agree on language coverage. Host note: `ci-build` is the *wrong* home — it deliberately
has zero lang-crate deps (`build_registry` takes a factory closure precisely to invert that
dependency); the shared body needs a new leaf crate above the lang crates.

### F9. The MCP server introduces itself as `codeindex-rs` — consistency

`ci-mcp/src/main.rs:2108`: `serverInfo.name = "codeindex-rs"` — client-visible identity,
two lines below a `[marksman-mcp]` log prefix. Nothing pins it (verified).

### F10. The live `apply_edits` description teaches a removed vocabulary — consistency

The tool description (`main.rs:1994,1998`) tells the agent ids come "by find_symbols,
list_anchors, retrieve_context, or a reject" and says "Never read_node/retrieve_context/
list_anchors the sites" — four names that stopped being tools when the two-tool facade landed
(they are `inspect` modes now). Same staleness class as file-surface F3 caught in the README, but
this copy is *live agent guidance*. Also stale: agent-facing error strings (`:488,584,730,834`)
and the module header (`:2-3`, still describes the six-tool surface). No test pins the prose;
`move_coverage_claims_come_from_the_provider_crates` (`:2358`) checks only MOVE_COVERAGE
containment.

### F11. Rollout providers don't advertise move coverage — gap

`MOVE_COVERAGE` exists for rust (`lang-rust/src/lib.rs:63`) and ts (`lang-ts/src/lib.rs:33`) and
is composed into the `apply_edits` description via placeholders (`main.rs:2044-2048`, pinned by
test). Java/php/swift do real cross-file move rewrites (F5!) yet say nothing — the agent can't
know a java move retargets importers.

### F12. Hardcoded `../../../codeindex` sibling path — consistency

`ci-embed/src/static_embedder.rs:146-148` points at the old Node repo by relative path. It is a
self-skipping parity-test helper (prints SKIP when absent), not production code — but it's the
one path in the tree that assumes a sibling checkout of the frozen prototype. An env override
with the current default keeps the oracle runnable without the assumption.

### F13. The foundational docs never caught up with the rollout — gap

The java/php/swift + container-gate work shipped in code and in its own specs, but:
`architecture.md`'s crate table (`:216-237`) omits all three rollout crates and still lists Java
among the *ungated* fallback languages (`:233`); `provider-contract.md`'s Tiers table (`:14-18`)
same; §8 (`:111-143`) is written future-tense ("belongs in ci-edit; when the next language lands,
extract") though `ci_edit::moves` shipped with four consumers; the contract is silent on the
sandbox seam and on gate-timeout semantics (which F1/F2 make load-bearing); `roadmap.md` Batch 8
(`:205-220`) doesn't record the rollout at all. A reader of the three foundational docs would
conclude Java is ungated and containers don't exist.

### Explicitly examined and cleared

Listed so the next audit doesn't re-litigate:

- **LSP quiescence/settle logic** — zero private copies; all three readiness modes
  (rust-analyzer `serverStatus`, jdtls `language/status`, sourcekit `$/progress`) live in
  `ci-lsp`, including the settle-race fix. The best-consolidated area in the tree.
- **Provider `lib.rs` delegation boilerplate** (~43 lines × 3 beyond template) — LEAVE. It
  cannot drift silently (a `LanguageProvider` change breaks all copies at compile time),
  `gated() -> true` is a contract-relevant declaration that should stay visible per crate, and
  the template is a pedagogical skeleton a macro would hide.
- **Sidecar bins** — first-pass audit called them byte-identical; verification says NOT so
  (provider type, log prefix, `gate_missing` arity differ; ~26 lines each). A macro would
  parameterize four things to save fifteen lines. LEAVE.
- **`CI_TIMING` blocks** (~9 sites) — each is a site-specific format string; the shareable part
  is one line. LEAVE.
- **ci-mcp dispatch & error envelope** — `inspect` is a thin 5-arm match; one JSON-RPC choke
  point wraps Ok/Err uniformly; the action enum and mode list are test-pinned to their
  dispatchers. No dedup available.
- **Bench-fixture-name policy** — HELD: zero fixture names in agent-facing strings; they appear
  only in code comments.
- **Dead code / TODO / suppressions** — none anywhere; one benign
  `#[allow(clippy::too_many_arguments)]`.
- **Test fixtures** — no cross-crate duplication (`tempfile` universal, inline fixtures);
  matches test-surface spec's cleared list.
- **`unwrap()`/`expect()` volume** — overwhelmingly `#[cfg(test)]`; remaining non-test
  `expect`s are just-configured-pipe invariants.

## 3. Proposals

### Phase 1 — shared gate plumbing (foundation, lands once)

**P1. `ci-core` gate-run helpers** (addresses F4, enables P2–P4)
`Error::GateTimeout(String)` (distinguished so callers can tell "hung tool" from "tool absent");
`GATE_OUTPUT_CAP = 32 MiB`; `run_gate_capped(sandbox, cmd, tool)` = `run_capped` at
`gate_timeout()`/cap with timeout ⇒ `Err(GateTimeout)`; `silent_tool_failure_diag(exited_ok,
parsed, anchor_file, first_line)` hoisting the F4 invariant (message extraction stays a
per-gate closure). Migrate php + swift gates onto both.
- Acceptance: behavior-identical — `cargo test -p ci-core -p lang-php -p lang-swift` green, php
  and swift `#[ignore]` gate tests green.
- Risk: low. Pure hoist; php/swift already have the semantics.

### Phase 2 — soundness fixes

**P2. Rust gate timed + capped; timeout REJECTS; delete `Sandbox::output`** (addresses F1)
`cargo_check_diags` → `run_gate_capped`; `RustEngine::diagnostics` matches `GateTimeout` and
*propagates* (only spawn-failure keeps the ra fallback). Then remove `Sandbox::output` from the
trait — the rust gate was its only production caller — so an untimed gate becomes
unrepresentable. New fast unit test: stub sandbox with `timed_out: true` ⇒ `Err(GateTimeout)`
propagates, no ra fallback.
- Acceptance: `lang-rust` fast + `-- --ignored` suites green; sandbox tests updated; the new
  timeout test proves no-degrade.
- Risk: the do-not-do is emitting timeout as a `Diag` — baseline-diff would excuse it on both
  passes (false pass). The design forbids it: timeout is an `Err` before baseline-diff runs.

**P3. Java gate deadline** (addresses F2)
Sidecar: reader thread + `mpsc`; `diagnostics()` does `recv_timeout(gate_timeout())`; on timeout
kill the child (stream desynced; next edit respawns) and `Err(GateTimeout)` — no fallback exists,
so it propagates. mvn/gradle classpath → `run_capped(gate_timeout(), 1 MiB)`, timeout ⇒ `None`
(classpath derivation's documented honest degrade, not the verdict).
- Acceptance: `lang-java` fast green; javac `#[ignore]` gate tests green (mvn/gradle e2e require
  those tools — record if unavailable).
- Risk: low-med. The reader thread changes sidecar plumbing; the EOF error path already handles
  post-kill calls honestly.

**P4. Swift target detection through the sandbox; G4 fail-closed** (addresses F3)
`describe_target_dirs(root, sandbox)` and `prime_index(root, sandbox)` via `tool_command` +
`run_capped`. `Ok(None)` only for "no Package.swift"; describe failure/timeout on a real package
⇒ `Err` with an actionable message — fail closed. `prime_index` stays best-effort.
- Acceptance: `lang-swift` fast + all `#[ignore]` (incl. `edit_to_file_outside_any_target_is_refused`)
  green; OCI swift e2e green if a runtime is present — under OCI the G4 check now *works* instead
  of silently skipping.
- Risk: low. A repo where describe fails almost always fails `swift build` too; the cost is an
  earlier, clearer error.

### Phase 3 — dedup

**P5. `ci_core::discover_tool(env, names, fallbacks)`** (addresses F7)
Publish the PATH walk; migrate jdtls/phpactor/sourcekit/phpstan discovery (phpstan keeps its
vendored-path middle step). rust/ts deliberately not migrated (unconditional-trust env
semantics — documented on the fn).
- Acceptance: fast workspace green; available rename e2es green.  - Risk: low.

**P6. `ci_edit::LazyLsp`** (addresses F6)
`get()` (starts on first use), `sync_disk`, `fs_events`, `will_rename_or(fallback)` (§8
ordering: LSP willRename when it starts and yields non-empty; else the movefix hooks; failed
start falls through silently — java/php's current shape). Java/php engines shrink verbatim;
swift keeps its `prime_index` specialization over `get()`; rust untouched. Home is legal:
ci-edit already depends on ci-lsp and owns `impl GateEngine for LspClient`.
- Acceptance: fast workspace green; available rename/move e2es green.  - Risk: low.

**P7. `DottedNameMoveModel` — the F5 twins become config** (addresses F5; the largest step)
New `ci_edit::moves::dotted`: `DottedSyntax` scalars (separator, import keyword/modifiers/stops,
alias rule, group-import rejects, trailing-separator semantics, source ext) + `DottedLang` hook
trait (file_to_ref, resolve, decl scan/render, insert anchor, masked_spans, deletion note —
masking is a hook because lang-fallback depends on ci-edit, not vice versa).
`line_start_offsets` moves to `ci-core` (re-exported from lang-fallback). `JavaMoveModel` /
`PhpMoveModel` become thin newtypes so **every existing movefix unit test compiles unchanged** —
the pinned span tests (trailing `.` is a reference, trailing `\` is not; php leading-`\`;
string/comment masking) are the behavior bar.
- Acceptance: all java/php movefix unit tests pass unchanged; java/php move e2es green.
- Risk: highest of the plan — the two boundary-rule asymmetries are exactly where a "generic"
  pass goes wrong; the pinned tests cover them. Run them first and often.

**P8. New crate `ci-providers`; one `make_provider`** (addresses F8)
Body = the mcp superset with a `log_prefix` param; both binaries delete their copies; per-binary
registry *policy* stays local. Effect on the cli: php/swift repos get the gated providers (or a
loud `Unavailable`) instead of silently-ungated fallback. Complementary to file-surface P1 (which
should not re-home this).
- Acceptance: workspace builds/tests green; mcp initialize + tools/list smoke on a fixture repo.
- Risk: low-med. The cli behavior change is the drift *removal*, stated here for sign-off.

### Phase 4 — consistency + docs

**P9. serverInfo → `"marksman"`** (F9). Acceptance: `cargo test -p ci-mcp` green.
**P10. Purge removed tool names from agent-facing text** (F10) — description, error strings,
module header, rewritten in `inspect`-mode vocabulary. Acceptance: `ci-mcp` tests green; grep
for the four names in string literals returns only code comments.
**P11. `MOVE_COVERAGE` for java/php/swift** (F11) — consts per crate next to the code that makes
them true; `%JAVA_MOVE%`/`%PHP_MOVE%`/`%SWIFT_MOVE%` placeholders in the description; extend the
pin test to all five languages. Acceptance: extended test green.
**P12. `CI_PARITY_REPO` override** (F12), current path stays the default, self-skip kept.
**P13. Doc reconciliation** (F13) — architecture.md crate table += three rollout rows and Java
removed from the ungated examples; provider-contract.md Tiers names java/php/swift, §8 rewritten
present-tense (extraction history kept as a note), new short clauses for the sandbox seam and
gate-timeout semantics ("a timed-out gate REFUSES — never passes, never swaps engines");
roadmap.md Batch 8 records the rollout. Acceptance: the spot-check claims in this spec's F13 all
read true afterward.

### Non-goals (explicit)

- No ci-edit mock consolidation, no ci-mcp `main.rs` split, no README/test-count fixes — owned by
  `test-surface-consolidation-spec.md` (T1/T7) and `file-surface-consolidation-spec.md` (P1/F3/F4).
- No `CI_` → `MARKSMAN_` env rename (open question 1).
- No re-typing of ci-mcp's `Result<String, String>` handler signatures (open question 2).
- No logging framework; `eprintln!` + prefixes stays.
- No provider-boilerplate macro, no sidecar-bin macro, no CI_TIMING helper (cleared above).
- No behavior change to rust/ts env-var trust semantics (F7 note).

## 4. Open questions for review

1. **Rename the `CI_` env prefix to `MARKSMAN_`?** ~30 vars, bench harnesses and container
   images read them. (Rec: keep `CI_`; the migration cost lands on every consumer for a purely
   cosmetic win. Revisit only if a public release forces it.)
2. **ci-mcp's stringly-typed `Result<String, String>` handlers** — unify on `ci_core::Error`?
   (Rec: defer to file-surface P1; re-typing 25 handlers is natural *during* the module split,
   double-work before it.)
3. **`ci-edit → ci-build` dependency** — the edit engine links the whole index builder. (Rec:
   examine during file-surface execution; a narrower interface may fall out of the split. Not
   load-bearing for this spec.)
4. **Thin crates `ci-scip` / `ci-lsp-index`** — fold into ci-index/ci-lsp? (Rec: leave; small
   but coherent, and ci-lsp-index is an ablation arm with its own lifecycle.)

## 5. Execution order & effort (once approved)

| Step | Items | Size | Gate |
|---|---|---|---|
| 1 | P1 plumbing + php/swift migration | S | fast ci-core/php/swift + their `#[ignore]` gate tests |
| 2 | P2 rust gate + `Sandbox::output` removal | M | lang-rust fast + `-- --ignored`; new timeout test |
| 3 | P3 java deadline | M | lang-java fast + javac `#[ignore]` tests |
| 4 | P4 swift sandbox + G4 | S | lang-swift fast + all `#[ignore]`; OCI if runtime present |
| 5 | P6 LazyLsp (before P7 — same files) | S | fast workspace + available rename e2es |
| 6 | P7 DottedNameMoveModel | M | java/php movefix units unchanged + move e2es |
| 7 | P5 discover_tool · P8 ci-providers | S/M | workspace build/test/clippy 0-warning + mcp smoke |
| 8 | P9–P12 consistency | S | ci-mcp + touched-crate tests |
| 9 | P13 docs + Status flip | S | spec F13 claims re-read true |

Every step additionally holds the standing bar: `cargo test --workspace` green,
`cargo clippy --workspace --all-targets` 0-warning, `ci-conformance` fast battery green.
