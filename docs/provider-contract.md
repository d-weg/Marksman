# The provider contract

What every language provider must guarantee, regardless of tier. This document is the prose
version; **the enforcement is executable** — `crates/ci-conformance` runs one shared battery
over every provider (`cargo test -p ci-conformance`; add `-- --ignored` for the providers that
shell out to real tools). A new language ships by adding its fixtures to
`ci-conformance/tests/conformance.rs` and passing, not by being reviewed carefully.

Gate-soundness clauses that need a live checker are enforced as e2e tests in the provider
crates (referenced per clause below); they are instances of this contract, not extras.

## Tiers

| tier | read path | edit gate | `gated()` | examples |
|---|---|---|---|---|
| **full** | SCIP (semantic) + tree-sitter | the language's compiler | `true` | TypeScript, Rust |
| **tree-sitter + gate** | tree-sitter + syntactic imports | the language's compiler | `true` | Java (javac sidecar), PHP (PHPStan), Swift (`swift build`) — each copied from `lang-template` |
| **ungated fallback** | tree-sitter + syntactic imports | syntax only (new parse errors reject) | `false` | Python, JS, Go, Ruby, C, C++ |

New languages enter at the tier the [rollout ladder](benchmarks.md#3-what-this-settles--the-provider-rollout-ladder)
prescribes: tree-sitter + gate first, a semantic read artifact as the maturity step. That
artifact comes from a dedicated SCIP indexer where a maintained one exists (scip-typescript,
`rust-analyzer scip`), or from the **`ci-lsp-index` sweep** (documentSymbol + references over
the language's own LSP → a genuine SCIP index) where none does — same consumer, same
conformance expectations; benchmarks.md §6.2 has the cost envelope (parity on fixtures, 38×
slower than a batch indexer at 379k-line scale, fine at the sizes most languages see). A
language without a usable checker stays ungated — honestly.

## 1. Identity & addressing

- Node ids are `file#Name`, nested scopes join with `.` (`file#Class.method`,
  `file#namespace.Type`), sub-nodes join with `:` (`:body`, `:params`, `:return`, `:doc`,
  `:param.N`). A child's id extends its parent's id — an id is unique AND self-locating.
- `name_range` slices the source to exactly the symbol's simple name (byte columns, 1-based
  lines — the `Range` contract is 0-based UTF-8 byte columns, matching tree-sitter).
- Function symbols expose `:body` and `:params` anchors (surgical-edit targets); a leading
  comment/docstring surfaces as `:doc`. Field/const ranges span the whole declaration
  statement, not just the name — the anchor must be editable as a unit.
- Sub-nodes lie within their symbol's range (`:doc` excepted — a leading comment sits above
  the declaration by definition).

## 2. Reads

- `structure()` is deterministic, cheap enough for the per-file indexing hot path, and fails
  SOFT on unknown files (empty or a clean error — never a panic).
- Reads stay true in-session: after a committed edit, `structure()`/`import_graph()` reflect
  it without a manual reindex (e2e: `committed_edit_refreshes_reads_in_session`, lang-ts).
  Cached indexes are validated by content fingerprint — **any doubt reindexes; a stale load is
  a correctness bug, a spurious reindex only a slow start.**

## 3. Import graph

- Keys and edges are repo-relative paths; the graph is deterministic.
- Edges are the language's REAL dependency edges or nothing: a language without a resolver
  returns an **empty** graph — an invented edge is worse than none (it feeds delete-safety and
  the blast radius). An honestly-empty graph is legitimate per language (Swift by module
  design; Go/Ruby/C/C++ until a resolver lands) — its consequence is that the gate's radius is
  exactly the edited files, and the tier's reply wording must not claim wider verification.
- **A syntactic graph must be served to the edit gate transitively**
  (`ci_core::transitive_reverse_imports`): syntactic edges do not flatten re-exports, so a
  one-hop radius lets a barrel hide its consumers — measured as a false "clean"
  (benchmarks §2.3) before this became contract. Semantic (SCIP) graphs may stay one-hop:
  they are already flattened, and the bounded radius is the hot path's cost control.
  (e2e: `barrel_consumer_inside_scip_blast_radius_outside_syntactic`, lang-ts.)
- Known limit of the syntactic tier (why SCIP is the maturity step): bare specifiers
  (`@acme/core`) produce NO edge — cross-package radius requires a compiler-resolved graph
  (e2e: `monorepo_bare_specifier_consumer_inside_scip_radius_invisible_to_syntactic`;
  benchmarks §2.4).

## 4. Edits

- `apply_edits` is the ONLY write entry point, and every provider routes it through the shared
  `ci_edit::commit_edits` spine (VFS staging → gate → commit-or-rollback). No provider-local
  transaction logic.
- The batch is **atomic**: one failing op rolls back everything — including ops that would
  have succeeded. A rejected batch leaves disk byte-identical.
- `dry_run` never writes. A missing anchor fails soft with the node named (no partial write).
- Same-file structural ops apply bottom-up (spans come from pre-batch disk truth); handled by
  the shared spine — a provider must not re-order or re-anchor ops itself.

## 5. The gate

- **Baseline-diff**: pre-existing breakage never blocks an unrelated edit; INTRODUCED breakage
  always rejects. What "breakage" means is the tier's honest capability: type diagnostics over
  the blast radius (gated tiers), new parse errors (ungated tier).
- The blast radius is sound for the graph being used (clause 3). Files a batch *creates* are
  materialized transiently for the check — a didOpen overlay at a not-on-disk path is
  invisible to a language server's project assignment.
- **The deletion convention**: an EMPTY buffer in the gate's `files` set is a deletion
  stand-in, and every materialization strategy must make that file *absent to the checker* so
  a surviving consumer's reference is the introduced break the gate catches. The four current
  spellings, each forced by its tool: rust never re-creates it during in-place staging; php
  withholds it from the analysis mirror; swift removes it from the package mirror; java's
  sidecar keeps it as an empty valid unit (javac's equivalent of absent). A new gate picks
  whichever spelling its tool forces — the convention itself is not optional.
- A rejection is **self-sufficient**: every new diagnostic carries the offending site's
  current source and, where derivable, a ready-to-copy `fix:` action anchored to the
  *post-edit* symbol. A response that tells the agent to "check it yourself" or re-derive
  addressing is a design bug (measured: benchmarks §2.5).

## 6. Honesty

- `gated()` reports what the gate actually verifies — never aspiration. Ungated results say
  so in the reply ("NOT type-verified"); the measured record shows an honest weak claim
  outperforming a false strong one, and a false "type-checked clean" shipping broken code.
- A missing toolchain disables the language **with an actionable install instruction**
  (startup, `doctor`, and any tool call touching its files). A provider never half-works and
  never silently degrades to a weaker gate (`ProviderBuild::Unavailable`, not a fallback).

## 7. Code consistency

- Shared spine, no local re-implementations: `ci_edit::commit_edits`,
  `ci_core::reverse_import_map` / `transitive_reverse_imports`, `ci_core::text::byte_offset`,
  `ci-treesitter` helpers. (The same-file-batch and byte-vs-char bugs both came from
  duplicated logic; the audit greps for reimplementations.)
- Crate layout: `crates/langs/lang-<x>/` — `lib.rs` (provider + `LanguageProvider` impl),
  engine modules as needed, a `marksman-provider-<x>` sidecar bin, tests in-crate (fast unit +
  `#[ignore]` real-tool e2e).

## 8. Moves & deletes — the reference-model contract

Two capabilities are language-specific only in their *syntax hooks*, not their shape — and
both are SHIPPED in the shared spine (`ci_edit::moves`), extracted when the java/php/swift
rollout produced the second consumer (the rule was "extract as the next language lands",
with `lang-rust`'s implementations as the reference semantics):

- **The move rewriter** (`ci_edit::moves::move_workspace_edit` over the [`MoveModel`] hook
  trait: `file_to_ref` / `ref_occurrences` / `membership_edits` / `is_source`). Its three
  concerns are universal: (a) how code REFERENCES a file (Rust `crate::` paths, TS relative
  specifiers, Java FQNs, PHP FQCNs), (b) how a file is DECLARED a project member (`mod x;` +
  `mod.rs`, a `package`/`namespace` line, implicit-by-dir), (c) rewriting (a) and
  maintaining (b) as one WorkspaceEdit. The generic engine (file walking, span edits,
  CreateFile ops, WorkspaceEdit assembly) lives in `ci-edit`; consumers today: rust
  (mod-decl model, the reference), java + php (both instances of the generic **dotted-name
  model**, `ci_edit::moves::dotted` — a `DottedSyntax` config + the `DottedLang` hooks:
  resolver, declaration scanner, string/comment masking), swift (the degenerate
  within-target no-op + Package.swift membership). Note every provider already implements
  the inverse of (a) — the syntactic import-graph resolver — so the hooks are small.
- **Deleted-reference diagnostics** (`ci_edit::moves::deleted_reference_diags`, the
  gap-fill for engines whose diagnostics miss unresolved imports). Generic form: resolve
  each surviving file's references through the provider's existing import resolver; any
  reference resolving to a batch-deleted path is a diagnostic. Wired in rust/java/php/swift.

Why this matters for the ladder: with these two shared, a new language gets complete
one-call moves and deletion soundness from its syntax hooks — the gate is the safety net
where one exists, and the diagnostics ARE the safety net where one doesn't. Engine-native
rewrites stay preferred where they exist (jdtls/phpactor `willRenameFiles`, tsgo/ts-morph
for TS; see `ci_edit::LazyLsp::will_rename_or` for the engine-first-then-hooks ordering);
the abstract rewriter is the fallback tier, exactly as movefix is for Rust.

The regression tests pinning the reference semantics live in the provider crates
(`lang-rust`'s committed-move-compiles / no-false-clean pins; the java/php movefix span
tests pin the dotted model's boundary rules — trailing `.` vs trailing `\`, leading-`\`
uses, string/comment masking).

## 9. Toolchain execution — the sandbox seam & timeout soundness

- Every gate/rename subprocess runs through the engine's `Sandbox`
  (`ci_core::resolve_sandbox(root, "marksman-<lang>")` at construction;
  `ci_core::tool_command` resolves each tool by bare name in a container, by host probe
  otherwise). No engine spawns a raw host `Command` for anything the verdict or a safety
  check depends on — under `CI_SANDBOX=oci` a host-only spawn silently runs the WRONG
  toolchain or fails a check open (the Swift G4 bug class). See `container-gate-spec.md`.
- **Gate verdict tools are time-bounded and output-capped** (`ci_core::run_gate_capped`:
  `gate_timeout()` — `CI_GATE_TIMEOUT_SECS`, generous by default — and `GATE_OUTPUT_CAP`).
  A timeout is `Error::GateTimeout` and REFUSES the edit: it never passes, never converts
  to a diagnostic (the baseline diff would excuse it on both passes), and never swaps in a
  weaker verdict engine. Resident gate processes bound their round-trips the same way
  (the java sidecar's `recv_timeout`).
- The reject-on-failed-tool invariant is shared (`ci_core::silent_tool_failure_diag`):
  a gate tool exiting non-zero with ZERO parsed diagnostics died before reporting — that is
  a REJECT, never a clean commit. Only the failure-message extraction is per-language.
- Non-verdict derivations (java's mvn/gradle classpath) may degrade honestly on
  timeout/failure — warn and fall back to the documented weaker behavior — because their
  failure mode is baseline-excused errors, not a false clean.

## 10. Read-artifact producers

An external tool that emits a read artifact consumed at open time (`scip-typescript`,
`rust-analyzer scip`) is a **producer**. Producers are adopted only at the rollout ladder's
maturity step — where their semantic edges are measured load-bearing (barrels, bare
specifiers, large-repo radius) — never as a new language's entry requirement. A producer
must:

- **Pin its version, and the pin participates in the source fingerprint** — bumping the tool
  reindexes on the next open; a stale artifact is never served under a new tool, and an
  unpinned fetch is the drift class that has twice changed behavior with no code change
  (`SCIP_TS_VERSION` is the reference shape).
- **Cache under `.marksman/`** with staleness decided by `ci_core::fingerprint` (content
  hashes, never mtimes). Doubt = reindex, never a stale read.
- **Serialize shared staging**: concurrent instances sharing an install cache (npx) hold the
  advisory lock (`NpxCacheLock` is the reference shape) so a racing install can't corrupt the
  producer out from under another process.
- **Keep stdout clean**: a producer's output stream must never reach the MCP/JSON-RPC channel.
- Fail LOUDLY: a producer that can't run disables its artifact path with an actionable
  message (contract §6) — it never silently serves the previous artifact as if fresh.

## 11. The workspace jail — reads and writes stay under root

Both IO layers refuse paths that escape the registered root, in the same two layers
(lexical: absolute-outside and `..` climbing above root; symlink: nearest existing
ancestor must canonicalize under root):

- **Writes** were always jailed at the edit chokepoint (`ci-edit`'s `ensure_within_root`
  covers every op path, node-id file, and move destination before any VFS mutation).
- **Reads** are jailed at `structure()` — the one place a caller-supplied path becomes a
  file read — via `ci_core::jailed_rel` (the write jail's twin). An out-of-root path has
  no nodes; inspect/read surfaces then answer not-found. Every provider inherits this
  through the three concrete `structure()` implementations (tree-sitter fallback, rust,
  ts); a new provider with its own `structure()` MUST gate its read on `jailed_rel`
  (conformance: `structure_read_is_jailed_to_root` in lang-fallback is the reference
  shape).

Rationale: marksman also serves embedders whose callers are semi-trusted (found live by
Foundry's daemon security review — its RPC layer jails independently, but the library must
not rely on every embedder remembering to). A local agent already has fs access, so this
is defense in depth there — and the actual boundary everywhere else.

## Adding a language (checklist)

1. Grammar + `classify` rows in `lang-fallback` (or a new crate at the gated tier).
2. Fixtures in `ci-conformance/tests/conformance.rs` — read + edit batteries green.
3. Registry entry (`LangSpec`: extensions, ignore dirs) + `doctor` toolchain probe if any.
4. Gated tier: a real `GateEngine` behind the same `commit_edits`; syntactic graph served
   transitively; gate e2e (reject type error / accept clean / rename lands cross-file).
5. Moves/deletes: implement the §8 reference-model hooks — and do the §8 extraction if this
   is the second consumer (movefix/deleted_path_references generalize out of lang-rust then).
6. Roadmap Batch 8 ladder: don't block on a SCIP indexer; don't claim more than the gate
   verifies.
7. Port the corpus fixture + `suites.<lang>` bindings in `scripts/agent-bench/tasks.json`
   (seven task identities, checkers verified fail-pre/pass-reference — see the bench README).
