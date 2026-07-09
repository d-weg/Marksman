# Language rollout: Java, PHP, Swift — spec for review

**Status: EXECUTING (2026-07-06). Preflight DONE (lang-fallback split verified; toolchain
probe: Swift ready locally, Java gate-half runnable, PHP fully tool-absent). JAVA PHASE
COMPLETE & verified: §8 extraction (generic move engine in ci-edit/moves.rs, lang-rust the
reference consumer, 16 rust e2e green), gated lang-java (resident javax.tools gate — real
reject/accept/atomic/baseline all pass), syntactic import resolver + reverse-radius reject
e2e (a type-break in a dependency rejects via the unchanged importer in the radius, real
javac). jdtls/mvn/gradle e2e blocked locally (ledger). PHP PHASE COMPLETE: lang-php (Composed ×
PHPStan-JSON gate + phpactor willRenameFiles + mock gate for the fast tier), PSR-4 resolver
(resolves-and-invents-nothing; composer-less = honest empty), namespace-membership MoveModel;
fast tier + conformance green, real-tool e2e (phpstan/phpactor) honestly #[ignore]-blocked
locally. SWIFT PHASE COMPLETE & FULLY VERIFIED against the real toolchain: lang-swift
(Composed × `swift build` gate — reject/accept/atomic/baseline all pass; sourcekit-lsp
cross-file rename passes), honest-empty within-target import graph, degenerate §8 no-op move
hooks with the gate as safety net (within-target move compiles). Whole rollout: workspace
0-warning, full non-ignored suite green, rust §8 reference suite (16) intact, §7 audit green.
FINALIZED 2026-07-06 (independent adversarial audit of PHP + Swift = both PASS; consolidating
gate/manifest/ledger produced). Two audit findings closed: deleted the orphaned
lang-fallback/src/java.rs (dead code, superseded by imports.rs), and added a REAL Swift
committed-within-target-move e2e (EditOp::MoveFile through the swift build gate — was only a
JSON-shape unit test). ALL UNCOMMITTED. Remaining: bench-suite ports (Q5, batched) + tool-gated
e2e (jdtls/mvn/gradle/phpstan/phpactor) need brew-installed toolchains.
Decisions recorded: Q1 = Java→PHP→Swift; Q2 = refuse-with-recipe for prefix-typed
return (LangSpec style only if demand appears); Q3 = phase-1 classpath derives from
mvn/gradle when present (one-command derivation), flat-javac otherwise, honest
Unavailable else; Q5 = bench-suite ports batched after all three providers land,
gated on local toolchain presence. All V-steps discharged 2026-07-06 by sourced web
research; residual implementation-time checks named inline (Swift grammar ABI load
test, PHP 8.5 pipe-operator parse, PHPStan JSON schema pin, phpactor namespace-rename
edges). Real-tool e2e per language runs only where the toolchain exists on the dev
machine — absences are reported, never papered over.**

## The ladder (settled policy, restated)

New languages enter at **tree-sitter + gate** (a real checker behind the shared
`commit_edits` spine), with a semantic read artifact (SCIP or LSP-sweep) as the later
maturity step; **ungated only for checker-less languages**. All three languages here
have usable checkers, so all three target the gated tier. Every provider assembles as
`Composed<FallbackProvider>` — exactly `lang-template`'s shape — plus a real
`GateEngine`. The registry is the single dispatch source: rows in `FbLang`
(lang-fallback), `Lang` (ci-walk), and `SUPPORTED` (ci-build registry) activate a
language everywhere with no per-binary wiring.

## What one language costs (the verified touchpoint map)

| Step | Where | Java | PHP | Swift |
|---|---|---|---|---|
| Grammar + classify rows | `lang-fallback/src/lib.rs` (FbLang enum :29, ALL :44, from_name :52, ts_language :71, exts :84, label :97, classify :281, outline kinds :249) | **already done** | new rows + `tree-sitter-php` dep | new rows + `tree-sitter-swift` dep |
| Lang detection | `ci-walk/src/lang.rs` (:6 enum, :21 of()) | **already done** | new | new |
| Registry | `ci-build/src/registry.rs` SUPPORTED (:120) | **already done** (`:165`, ungated) | new LangSpec | new LangSpec |
| Syntactic import resolver | `lang-fallback` `file_imports` (:138 — currently Python/Js/Ts only) | new: `import a.b.C` → `a/b/C.java` under source roots | new: PSR-4 (`use` → composer autoload map) | none needed (module-level imports; see below) |
| Provider crate | `crates/langs/lang-<x>/` = template copy + engine | new `lang-java` | new `lang-php` | new `lang-swift` |
| Conformance | fixtures in `conformance.rs` (java ungated exists :159; template gated pattern :322) | upgrade to gated fixture | new read+edit fixtures | new read+edit fixtures |
| Toolchain probe | `toolchain()` + doctor (§6: Unavailable-with-hint, never silent fallback) | javac/jdtls probes | php/phpstan/phpactor probes | swift/sourcekit-lsp probes |
| Bench | `tasks.json` `suites.<lang>` + `fixture-<lang>` (7 task identities) | new suite | new suite | new suite |

Grammar ABI is a non-issue: grammar crates link through the `tree-sitter-language`
shim (verified: `tree-sitter-java` 0.23.5 works against core 0.26.10).

## Per-language assessment

### Java — a tier upgrade, not an addition (recommended FIRST)

Already fully wired at the ungated tier. What's missing is the write half and the graph:

- **Gate** [V1 DISCHARGED 2026-07-06, web-verified]: `javac` has NO structured CLI
  diagnostics (explicit OpenJDK non-goal) and Maven has no trustworthy incremental
  compile (warm full-module ≈11-12s); Gradle incremental ≈5s warm — all poor per-edit
  gates. The verified better fits: **ecj** (Eclipse batch compiler: `-log *.xml`
  structured diagnostics out of the box, incremental-first architecture) or a small
  resident **`javax.tools.JavaCompiler`** wrapper (in-process `DiagnosticListener` →
  kind/source/position/code, no text parsing) — the same resident-sidecar shape as
  ts-morph. Caveat recorded: ecj is a genuinely separate compiler with rare javac
  divergences — per the fix-the-expected-engine policy, prefer the `javax.tools`
  wrapper (IS javac) when a JDK is present; classpath still derives from the build tool.
- **Rename / willRename engine** [V1 DISCHARGED]: jdtls confirmed the de-facto standard
  (v1.60.0, June 2026; requires a **Java 21+ runtime** — deployment constraint for the
  toolchain probe). `willRenameFiles` VERIFIED since v1.35: moving a `.java` file
  rewrites the package decl AND importers — engine-native moves exist, the §8 abstract
  rewriter is Java's fallback tier only. Two integration facts: (1) willRename is
  client-driven — we must advertise `fileOperations.willRename` and apply the returned
  WorkspaceEdit ourselves (ci-edit already works this way); (2) jdtls has **NO LSP 3.17
  pull diagnostics** (push-only through v1.60.0) — the gate needs publish+settle
  quiescence logic, the same class of problem already solved for rust-analyzer, OR
  diagnostics come from the ecj/javax.tools gate and jdtls does only rename/willRename.
  Cold start is bimodal: ~8-15s warm with a persisted workspace, MINUTES on first
  import of a real Maven/Gradle repo — persisting the jdtls workspace dir per repo and
  `prewarm` are both load-bearing.
- **Import graph**: Java is the friendliest case in the whole rollout — `package`
  declarations must match directory paths, and imports are class-level. A syntactic
  resolver (`import a.b.C` → `<source-root>/a/b/C.java`) is deterministic given source
  roots (`src/main/java` conventions + fallback to repo scan). Real edges per contract
  §3, served transitively by Composed (`semantic_edges()=false`).
- **§8 hooks** (moves/deletes): `file_to_ref` = path→FQN; `ref_occurrences` = imports +
  FQN mentions; `membership_edits` = rewrite the `package` line to match the new dir.
  Closest analog to Rust's model → **best validator for the §8 extraction**.
- **SCIP later** [V1 DISCHARGED]: scip-java verified alive (human commits July 2026,
  v0.12.3) but it indexes by injecting a compiler plugin into a REAL build — broken or
  exotic build ⇒ no index. Same trade as scip-typescript, heavier coupling; correctly
  deferred to the maturity step.
- **2025-26 landscape check**: Oracle's javac-based VS Code extension is the one real
  new entrant but ships only as a VS Code extension, not a standalone LSP — jdtls
  remains the headless default. No structured-diagnostics JEP for javac.
- **Known op wrinkle**: Java's return type *precedes* the method name — the
  `set_return_type` handler's insert-after-`)` + delimiter model produces garbage.
  Decision needed (open question 2): (a) refuse the op for prefix-typed languages with
  a self-sufficient error steering to `replace_text` on the signature — cheap, honest;
  or (b) grow LangSpec with a return-type *style* (suffix/prefix) and teach the handler
  prefix replacement. Recommendation: (a) now, (b) only if bench/usage shows demand.

### PHP — the licensing-aware pick (SECOND)

- **Grammar** [V2 DISCHARGED 2026-07-06, web-verified]: `tree-sitter-php` 0.24.2
  (official tree-sitter org, links via the shim — no core-pin conflict); the php /
  php_only split confirmed (crate exports both) — use `LANGUAGE_PHP` (full, handles
  HTML interleaving) for `.php`. One implementation-time check: 0.24.2 predates PHP 8.5
  (Nov 2025) — verify the pipe-operator (`|>`) parses before trusting fingerprints/
  structure on 8.5 code; parse-failure must stay soft per contract §2.
- **Gate** [V2 DISCHARGED]: **PHPStan confirmed** the de-facto standard (2.2.x, full
  PHP 8.5 support; Psalm is down to a single maintainer — projects migrate Psalm→
  PHPStan, not the reverse). Verified mechanics: `--error-format=json` per-file output
  (the exact schema is NOT documented in writing — pin our parser against observed
  output with a fixture test); runs config-less via CLI (`analyse <paths> --level N`);
  without `composer install` it degrades GRACEFULLY to explicit unknown-symbol
  diagnostics rather than crashing — which means the gate must baseline-diff those away
  (they are pre-existing state, exactly the §5 clause). Result cache makes warm runs
  fast; cold is seconds — measure on fixtures. No PHPStan → ungated and says so (§6).
- **Rename / willRename engine** [V2 DISCHARGED — better than assumed]: **phpactor**
  verified very healthy (commits through 2026-07-05, PHP 8.5 support) and it
  **implements `workspace/willRenameFiles` as real LSP fileOperations**
  (source-verified: `FileRenameHandler` registers the capability and returns a
  WorkspaceEdit rewriting class name/namespace + references on file move) — PHP gets
  engine-native moves through the standard LSP channel, no custom RPC. Rename covers
  variables + members + classes (the readthedocs support table is stale; the
  `ClassRenamer` is registered in source; class rename also file-renames via workspace
  edit). Verify namespace-rename edge cases empirically in the e2e. Install story:
  **PHAR from GitHub releases** (not composer global); needs a PHP runtime with posix.
  Intelephense exclusion CONFIRMED justified: rename and the file/folder-rename that
  rides on it are premium-licensed ($35/$75 per user).
- **Import graph**: PSR-4 — composer.json autoload map gives a deterministic
  `use X\Y\Z` → file mapping. Repos without composer.json get the honest empty graph
  (contract §3: invented edges are worse than none).
- **§8 hooks**: `file_to_ref` = path→namespace via PSR-4 inverse; `membership_edits` =
  rewrite the `namespace` line; refs = `use` statements + FQNs. Second §8 consumer
  shape, different membership mechanism than Java — good generality test.
- **SCIP later** [V2 DISCHARGED]: scip-php (davidrjenni) exists and is what Sourcegraph
  lists for PHP, but it is 0.0.x with thin maintenance and hard-requires a healthy
  composer/vendor state — usable-but-immature; do NOT put it on the critical path.
  Gate-first rollout confirmed; the LSP-sweep remains the fallback semantic path.
- **2025-26 landscape check**: an "official PHP language server" is a proposal under
  discussion (Oct 2025), not a shipped project — don't design against it. PHPantom (a
  pre-1.0 Rust PHP LSP with impressive startup/memory numbers) is worth a watch as a
  future engine candidate, not a dependency today.

### Swift — the structurally novel one (THIRD)

- **Grammar** [V3 DISCHARGED 2026-07-06, web-verified]: `tree-sitter-swift` 0.7.3
  (June 2026, actively maintained, 3.3M downloads) depends on the `tree-sitter-language`
  shim only — no conflict with our core 0.26.10 pin. One residual check at
  implementation time: a one-line grammar-load test (ABI-14-in-core-0.26 is inference
  from tree-sitter's compat policy, not a doc).
- **Gate** [V3 DISCHARGED]: `swift build`, **full stop** — `swiftc -typecheck` is
  REFUTED as a gate by the core team itself: it misses SIL-phase diagnostics
  (definite-initialization, some exhaustiveness), i.e. it would be an unsound gate, the
  exact false-clean failure mode the contract exists to prevent. Diagnostics are
  regex-parsed GCC-style text (no JSON; the JSON-diagnostics issue is still open;
  `-serialize-diagnostics-path` emits LLVM bitstream — not worth a reader). Latency:
  no authoritative warm-build numbers exist and Swift 6.3's new Swift Build engine
  (SwiftPM preview) may shift them — benchmark on our fixtures, don't spec a number;
  known hazard: build-tool plugins can re-run per incremental build. Toolchain pinning
  via **swiftly** (official manager, macOS + Linux).
- **Rename engine** [V3 DISCHARGED — the historical concern is RESOLVED]: sourcekit-lsp
  rename is production-quality since the Swift 6 toolchain — cross-file, prepareRename,
  served from the IndexStoreDB global index with **background indexing on by default
  since 6.1**. Rename correctness = index freshness (same staleness class the Rust gate
  already handles). Two integration facts: (1) pull diagnostics (LSP 3.17) ARE
  supported but registered **dynamically** — ci-lsp must advertise
  `textDocument.diagnostic.dynamicRegistration: true` and handle
  `workspace/diagnostic/refresh`, else it silently degrades to publish mode; (2)
  `willRenameFiles` is REFUTED (no handler exists) — moot within a target (see module
  model), and cross-target moves are Package.swift membership edits our §8 hook covers.
- **Module model — the interesting part**: Swift imports are MODULE-level, not
  file-level, and SwiftPM targets glob directories. Consequences: (1) the syntactic
  import graph degenerates — within a target there are no file-file edges to extract, so
  the honest graph is empty-or-target-level, and the blast radius must widen to the
  TARGET for gating (the gate compiles the package anyway, so soundness holds); (2) the
  §8 hooks are near-no-ops within a target — files aren't referenced by path and need no
  membership declaration; only cross-target moves touch `Package.swift`. Swift is the
  degenerate-case validator for the §8 extraction: it proves the hooks may legally be
  no-ops.
- **SCIP later** [V3 DISCHARGED]: confirmed NO SCIP indexer for Swift exists. Two
  maturity paths, in preference order: the **ci-lsp-index sweep** over sourcekit-lsp
  (references/documentSymbol are served from the same IndexStoreDB the background
  indexer keeps warm — architecturally sound, with the known per-query scale cliff our
  TS sweep measurements predict), or reading **IndexStoreDB directly** (the canonical
  semantic source; higher fidelity, more integration work). The sweep gives
  `ci-lsp-index` its second consumer either way (the file-surface spec's watch item
  resolves itself). sourcekit-lsp caveat: SwiftPM projects only — no Xcode-project
  support; the provider's toolchain probe should say so.

## The §8 extraction (shared, triggered by Java)

The contract mandates it "as the next language lands" with lang-rust as reference
semantics. Concretely, in the Java phase:

1. Generic engine in ci-edit (file walking, span edits, CreateFile ops, WorkspaceEdit
   assembly — extracted from `lang-rust/src/movefix.rs`), consuming three provider
   hooks: `file_to_ref(path)`, `ref_occurrences(content)`, `membership_edits(from, to)`.
2. `deleted_path_references` generalizes the same way (resolve surviving files' refs
   through the provider's import resolver; deleted target ⇒ diagnostic) — this also
   discharges the test-surface spec's plan to route the `delete_file` import check
   through provider resolvers (its P9 trigger is exactly this moment).
3. lang-rust reimplements movefix/deleted-refs AS the hooks; its regression suite
   (committed-move-compiles, no-false-clean, delete-refusal recipe) pins that the
   generic form preserved reference semantics. PHP then implements the hooks (or uses
   phpactor engine-native moves, preferred where they work); Swift implements the
   no-op/degenerate form.

## Sequencing & dependencies

```
0. Preflight: file-surface P2 (lang-fallback split) BEFORE any rows land there
   (V1/V2/V3 toolchain verification: DONE 2026-07-06 — folded into this spec)
1. JAVA  — tier upgrade + §8 extraction (the big shared payoff) + bench suite
          gate = resident javax.tools wrapper; rename/moves = jdtls (push-diag only)
2. PHP   — full addition; §8 hooks second consumer; phpactor engine-native moves
          (real LSP willRenameFiles, source-verified); gate = PHPStan JSON
3. SWIFT — full addition; degenerate §8 case; gate = swift build (typecheck-only is
          UNSOUND); rename = sourcekit-lsp (production since Swift 6);
          ci-lsp-index sweep as maturity step
```

Each language phase ends at the same bar: conformance fixtures green (fast tier),
in-crate real-tool e2e green (gate reject/accept, rename lands cross-file, move
completeness), toolchain probes honest, bench suite ported with checkers verified
fail-pre/pass-reference. No language claims more than its gate verifies.

## Non-goals

- No SCIP indexers in the initial phases (maturity step, per the ladder).
- No Intelephense (licensing); no invented import edges for composer-less PHP or
  within-target Swift; no `set_return_type` contortions for Java beyond the chosen
  option in Q2.
- No speculative §8 generality beyond what the three real consumers demand.

## Open questions for review

1. **Sequencing**: Java → PHP → Swift as argued (risk-ordered, §8 payoff first)?
2. **Java `set_return_type`**: refuse-with-recipe (rec) or LangSpec return-style?
3. **Java gate shape** (narrowed by V1): the verified recommendation is a resident
   `javax.tools.JavaCompiler` wrapper (structured diagnostics, IS javac) with
   build-tool-derived classpath; ecj is the alternative if a JDK-less deployment
   matters. Remaining decision: is classpath-derivation-from-build-tool (parse
   `mvn dependency:build-classpath` / Gradle equivalent) in phase 1, or do flat/simple
   classpaths gate first with build-tool derivation staged?
4. ~~**Swift rename**~~ — DISSOLVED by V3: sourcekit-lsp rename is production-quality
   since Swift 6 (cross-file, index-backed, background indexing default since 6.1). No
   tiering or hold needed.
5. **Bench fixtures**: port the full 7-identity corpus per language in-phase (contract
   checklist step 7), or land provider + conformance first and batch the bench ports?
