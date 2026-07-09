# Java / PHP / Swift rollout — audit spec (findings for review)

## Resolution status (2026-07-08 — fixes applied, all UNCOMMITTED)

Fixes were applied item-by-item after the audit below; workspace stays **0-warning**, the **full
non-ignored suite passes (0 failed)** across every crate, and the real-tool ignored e2e are green
(**Java 9/9** incl. the new move e2e with real `javac`; **Swift 7/7** with real `swift build`,
including the new G4 out-of-target refusal). Two passes: the first cleared the critical/high
findings, the second cleared every formerly-deferred item except G7 (see below).

**Fixed & verified** — G1, G2, G3 (reject-on-failed-tool in the Swift & PHP gates, mirroring
`lang-rust`'s inline invariant; PHP now also captures the top-level `errors` array and no longer
nulls stderr) · G5 (Swift diagnostics deduped by `(file,line,message)`) · B1 (`Composed` drops a
cached engine on a gate `Err` so a dead resident sidecar respawns — its promise is now true) · R1
(PHP PSR-4 longest-prefix now owns the FQCN; no fall-through to a shadow file) · R2 (Java resolver
verifies the target declares the import's package — no path-coincidence edges) · R3 (Java
`import static a.b.C.member` drops the member, resolves the type) · M2 (PHP leading-`\` `use` now
rewritten + noted) · M3 (default-package / global-namespace moves ADD the decl via `InsertAt`,
Java + PHP) · M4 (inline `<?php namespace App;` recognized, in one shared scanner) · C1 (PHP probe
now calls the real `gate::phpstan_binary` resolver, `root`-threaded) · C2 (Java uses the shared
`lang_template::mock`) · C3, D3 (Swift/facade doc fixes) · D1 (package/namespace decl scanners
consolidated into `imports.rs` `package_decl`/`namespace_decl`; dead re-exports dropped) · **M1** (Java & PHP no
longer rewrite a fully-qualified name inside a string literal or comment — the idiomatic
tree-sitter approach: a shared `lang_fallback::string_comment_spans` masks those exact byte
extents, and the movefix code-mention scan skips any run starting inside one; the proven
longest-prefix resolution and the unmasked import/`use` branch are untouched. Contract: string
contents are never rewritten, so reflection/DI strings won't auto-update — a documented, safe
limit. The `lang-rust` reference matcher is deliberately left as-is per its §8-oracle status).
New regression tests: R1, R2, M2, M3 (Java+PHP), M1 (Java+PHP string/comment masking), plus the
audit's own test-gaps **T2** (Java committed cross-package move through real javac) and **T3**
(deleted-reference diagnostics, Java+PHP).

**Dismissed as consistent-with-Rust/TS** (per reviewer guidance "behave as close as TS/Rust as
possible") — S1 (build-execution on untrusted repos: `cargo check` already runs `build.rs`/proc-
macros; no opt-out knob added) · G6 (Java flat-classpath degrade keeps `gated()` + `eprintln!`,
exactly like `lang-rust`'s ra fallback).

**Non-issue** — D2 (the `hostile_imports.rs` debug `println!` was a transient file from a
concurrent audit agent; no such file exists in the tree).

**Second pass (also fixed & verified)** — the formerly-deferred items, in severity order:
**G4** (Swift edit to a file outside every SwiftPM target now REFUSED, not committed under a false
"type-verified" claim — target membership from `swift package describe`, cached; Swift's empty
import graph means the gate's file set is exactly the edited files, so the refusal is an `Err` that
propagates before baseline-diff can cancel it; real-`swift build` e2e). **M5 / D4** (Swift model is
now target-aware — a cross-target move DECLINES honestly instead of a false "handled" empty edit;
the dead `&Path` field is gone, model is a unit struct). **T1** (PHP baseline-diff + atomicity
`#[ignore]` e2e added — kept INTRA-FILE so they don't depend on PHPStan's cross-file resolution;
they mirror the passing Java analogs exactly, compile clean, and skip loudly where phpstan is
absent). **B2** (Swift mirror copy now skips `.swift` files outside any target, reusing G4's cached
describe — the cold full-package *build* stays as the intrinsic soundness cost). **B3 + B4** (one
shared `ci_core::run_capped`: stdout/stderr capped at 32 MiB so a chatty tool can't OOM us, and a
generous env-overridable `CI_GATE_TIMEOUT_SECS` (default 600 s) so a hung tool can't hang the edit
forever while never killing a legitimately slow build; concurrent pipe-drain avoids deadlock;
unit-tested for both truncation and timeout, and validated end-to-end through real `swift build`).
**G8** (`split_diag_head` anchors on the `:LINE:COL:` numeric shape, so a path containing `": "`
no longer silently drops a diagnostic).

**Consciously not fixed** — **G7** (Swift cold-index rename message): the "distinguish index-cold
in the reject" fix would require threading rename-freshness state into a `swift build` reject that
structurally has none — an awkward contortion for a message nicety; the gate still catches the real
break, so this stays as-is with a note rather than forced.

---

**Status: AUDIT PHASE — findings only, NO fixes applied.** This is the spec-driven,
multi-agent flaw & consistency audit of the three new gated providers (`lang-java`,
`lang-php`, `lang-swift`) and the shared §8 move engine (`ci-edit::moves`), held to the
bar the mature `lang-rust`/`lang-ts` providers and `docs/provider-contract.md` set. All
subject code is UNCOMMITTED. Reviewer (Davi) triages this list; then we orchestrate fixes
item-by-item (impl → independent adversarial verify → fix-once-or-abort), as before.

Eight adversarial sub-agents ran, one per dimension (gate soundness, resolver correctness,
MoveModel, cross-provider consistency, sidecar security, robustness, dead code, test
quality). The most-severe findings — the gate false-clean holes and the string-literal
over-match — were independently re-verified against source during synthesis.

---

## Terms

- **False-clean** — the gate returns "no new errors, commit" over an edit that is actually
  broken. The spine (`ci-edit/src/lib.rs:696`) treats an **empty** post-edit diagnostic set
  as clean-commit, so *any* gate that returns `Ok(vec![])` on a tool **failure** (nonzero
  exit, crash, headless error) is a false-clean. This is the single worst failure class —
  it ships broken code under a "type-checked" claim (contract §5, §6).
- **Invented edge / invented edit** — an import-graph edge, or a move rewrite, to a path
  that is NOT a real dependency. Feeds delete-safety and blast-radius; contract §3 says an
  invented edge "is worse than none." An invented *edit* (rewriting the wrong bytes) that
  still compiles is gate-invisible — the gate cannot catch it.
- **The gate-can't-catch rule** — the fallback move rewriter is "best-effort, never
  silently wrong" ONLY because "the type-check gate rejects any rewrite that comes out
  wrong" (`ci-edit/src/moves.rs:14-18`). Any wrong rewrite that still **compiles** breaks
  this guarantee.
- **Reject-on-failed-tool invariant** — the reference Rust gate encodes it explicitly
  (`lang-rust/src/gate.rs:107-111`): *tool exited nonzero but we parsed no diagnostics →
  synthesize a reject, never read silence as clean.* Two of the three new gates dropped it.
- **§N** — clause N of `docs/provider-contract.md`.
- **Verdict tags** — `[REAL FLAW]` (a defect to fix), `[CONSISTENCY DRIFT]` (diverges from
  how Rust/TS do the same thing, no language reason), `[OVERLAP-BY-DESIGN, keep]`
  (divergence justified by a real language difference), `[DECISION NEEDED]` (a policy call
  the reviewer must make).

---

## Severity summary

| # | Finding | Provider | Verdict | Sev |
|---|---|---|---|---|
| **G1** | Swift gate never checks `swift build` exit status → link/manifest/toolchain failures commit false-clean | swift | REAL FLAW | **critical** |
| **G2** | PHPStan gate never checks exit status + empty-stdout read as clean → analyser crash false-clean | php | REAL FLAW | **critical** |
| **G3** | PHPStan top-level `errors` array dropped → internal/config errors false-clean | php | REAL FLAW | **critical** |
| **M1** | Java & PHP rewrite FQNs inside string literals and comments → invented, gate-invisible edits | java, php | REAL FLAW | **critical** |
| **R1** | PHP PSR-4 longest-prefix fall-through invents an edge → corrupts delete-safety | php | REAL FLAW | high |
| **G4** | Swift gate silent on files outside the compiled target → un-gated edit claims "clean" | swift | REAL FLAW | high |
| **G5** | Swift diagnostics not deduped × count-based baseline diff → false-REJECT of pre-existing errors | swift | REAL FLAW | high |
| **B1** | Java resident sidecar wedges for the session; its error promises a respawn that never happens | java | REAL FLAW | high |
| **M2** | PHP `use \App\Foo;` (leading `\`) → lost rewrite AND lost deletion diagnostic | php | REAL FLAW | high |
| **M3** | Default-package / global-namespace move silently declines the whole move (no InsertAt-to-add) | java, php | REAL FLAW | high |
| **M4** | PHP inline `<?php namespace App;` invisible → cross-namespace move declines; same blind spot in the resolver | php | REAL FLAW | high |
| **T1** | PHP has no real-gate e2e for baseline-diff / atomicity (soundness clauses untested) | php | REAL FLAW | high |
| **T2** | Java has ZERO move test (fast or e2e) — §8 committed-move uncovered | java | REAL FLAW | high |
| **R2** | Java flat-layout root resolves imports by path coincidence, no package check → invented edges | java | REAL FLAW | medium |
| **S1** | Gating executes repo-controlled build code; undocumented, no opt-out | all | DECISION NEEDED | medium |
| **G6** | Java flat-classpath degrade can mask introduced errors while still claiming `gated()` | java | DECISION NEEDED | medium |
| **B2** | Swift gate deep-copies the whole repo tree on every edit (scaling cliff) | swift | REAL FLAW / DECISION | medium |
| **C1** | PHP toolchain probe (PATH-only) disagrees with the gate resolver (vendor/`$CI_PHPSTAN`) → false "disabled" | php | REAL FLAW | medium |
| **D1** | Dead resolver re-exports + movefix reimplements `package_of`/`namespace_of` instead of calling the shared facade (§7) | java, php, fallback | REAL FLAW / DRIFT | medium |
| **T3** | Deleted-reference diagnostics wired into Java & PHP `diagnostics()` but never tested at provider level | java, php | REAL FLAW | medium |
| **R3** | Java `import static a.b.C.member` misses the real edge | java | REAL FLAW (miss) | low |
| **M5** | Swift cross-target move reports a "handled" empty edit while doing zero `Package.swift` work | swift | DECISION NEEDED | low |
| **B3** | Unbounded subprocess stdout/stderr capture (PHP & Swift gates) | php, swift | REAL FLAW | low |
| **B4** | No wall-clock timeout on `swift build` / PHPStan subprocess (drifts from the LSP client's discipline) | php, swift | CONSISTENCY DRIFT | low |
| **G7** | Swift `prime_index` swallows a failed root build → cold-index rename rejects mysteriously | swift | CONSISTENCY DRIFT | low |
| **G8** | Swift `split_diag_head` splits on the first `": "` → drops a diagnostic whose path contains `": "` | swift | REAL FLAW | low |
| **C2** | Java hand-rolls `MockChecker`/`MARKER` instead of the shared `lang_template::mock` | java | CONSISTENCY DRIFT | low |
| **C3** | Swift gate module doc contradicts the code on stdout-vs-stderr | swift | REAL FLAW (doc) | low |
| **D2** | Debug `println!` left in a committed test | fallback | REAL FLAW | low |
| **D3** | Stale facade doc names a nonexistent `file_to_class` | fallback | DRIFT | low |
| **D4** | `SwiftMoveModel` dead `&Path` field behind `#[allow(dead_code)]` | swift | DECISION NEEDED | low |

---

## Critical

### G1 — Swift gate never checks `swift build`'s exit status → false-clean on any headless build failure
**[REAL FLAW]** · `crates/langs/lang-swift/src/gate.rs:45-63`, `parse_swift_diagnostics:103-134`

```rust
let out = Command::new("swift").arg("build") … .output()…?;
// out.status is NEVER consulted
let combined = format!("{}\n{}", stdout, stderr);
Ok(parse_swift_diagnostics(&combined, mirror.path()))
```

`parse_swift_diagnostics` keeps only lines matching `PATH:LINE:COL: error:`. Many real
build failures exit nonzero with **zero source-anchored `error:` lines** — the agent
reproduced these live on this machine (swift 6.1.2):
- **Link error** (`@_silgen_name` to a missing symbol): `EXIT=1`, output is
  `error: link command failed…` / `Undefined symbols…` / `ld: symbol(s) not found` — no
  `file:line:col` head. Parser returns `[]` → `diagnostics()` returns `Ok(vec![])` → spine
  sees `after.is_empty()` → **commits the broken edit as clean.**
- **Manifest failure**: `error: 'pkg': Invalid manifest …` — headless, skipped identically.

The passing ignored tests only exercise *type* errors (which carry a source head); the
whole failure class is untested. This is exactly what the Rust gate synthesizes a reject
for; Swift dropped the defense.

**Fix**: mirror `lang-rust/src/gate.rs:107-111` — `if !out.status.success() && diags.is_empty()`,
push a synthetic reject `Diag` (file `Package.swift`, line 0) carrying the first non-empty
stderr/stdout line, so a headless build failure REJECTS. (See the shared-helper note under
G2/G3.)

### G2 — PHPStan gate ignores exit status and treats empty stdout as clean → false-clean on analyser crash
**[REAL FLAW]** · `crates/langs/lang-php/src/gate.rs:72-86`, `parse_phpstan_json:92-96`

```rust
.stderr(Stdio::null())            // fatal text on stderr is discarded
.output()…?;                      // out.status never consulted
let stdout = String::from_utf8_lossy(&out.stdout);
parse_phpstan_json(&stdout, dir.path())
…
if trimmed.is_empty() { return Ok(Vec::new()); }   // empty stdout == clean
```

If PHPStan dies before emitting JSON (segfault, OOM-kill, a fatal in a rule/extension, a bad
`--level`, no PHP runtime for the phar), it exits nonzero with empty stdout and its message
on the now-nulled stderr → `Ok(vec![])` → false clean. The in-code comment claims "only a
genuinely unparseable stdout is an error," but *empty* stdout is the realistic crash shape
and is silently treated as "no findings." Cannot verify live (php absent), but the code path
is unambiguous.

**Fix**: capture `out.status`; when `!success && trimmed.is_empty()`, return a synthetic
reject / `Err`. Do not null stderr — fold its first line into the surfaced message.

### G3 — PHPStan top-level `errors` array is dropped → internal/config errors commit false-clean
**[REAL FLAW]** · `crates/langs/lang-php/src/gate.rs:106-124`

```rust
let Some(files) = v.get("files").and_then(|f| f.as_object()) else {
    return Ok(out); // no per-file block => nothing actionable
};
```

PHPStan's JSON has both `files` and a top-level `errors` array for non-file-scoped problems
("Internal error", config/ignore mismatch, "Reflection error"). The parser reads only
`files`; a run whose only output is a populated top-level `errors` → `Ok(vec![])` → false
clean, even though PHPStan is telling you it could not analyze. The unit fixture literally
carries `"errors":[]` and never exercises a populated one.

**Fix**: after the `files` loop, if `v["errors"]` is a non-empty array, emit each as a `Diag`
(file `""`/`phpstan`, line 0) so an analyser-level failure rejects.

> **G1+G2+G3 share one root and one fix.** All three drop the reject-on-failed-tool
> invariant. A single shared helper — `fn reject_if_tool_failed(status, stdout, stderr,
> diags)` in `ci-edit` — applied in the Swift and PHP gates closes all three and restores
> consistency with `lang-rust`. Add a link-error (Swift) and a killed-analyser (PHP) fixture
> to `ci-conformance` so the invariant is enforced for future languages (contract
> §Adding-a-language). This is the top-priority fix cluster.

### M1 — Java & PHP rewrite FQNs inside string literals and comments → invented, gate-invisible edits
**[REAL FLAW]** · `crates/langs/lang-java/src/movefix.rs:98` (`dotted_runs`), `crates/langs/lang-php/src/movefix.rs:102` (`backslash_runs`)

The code-mention scan runs on *every* non-`import`/`use` line with **no lexical state** —
string literals and comments are scanned identically to code. Independently verified during
synthesis:
- Java: `String msg = "see com.x.Helper docs";` → `dotted_runs` yields `com.x.Helper`; if it
  resolves to a source file (it does — it's the moved class), a rewrite span with `note:
  None` is emitted and `move_workspace_edit` splices the new FQN **inside the string
  literal**. Same for `// uses com.x.Helper`.
- PHP: `fqcn_spans("'App\\Foo'", "App\\Foo")` → `[(1,8)]` — rewrites inside a quoted string.

The design leans on "the compiler gate rejects a wrong rewrite" — but a class name rewritten
inside a **comment** or a **reflection/logging string** (`Class.forName("com.x.A")`, a log
message) still **compiles**. javac/PHPStan pass, and the move has silently corrupted a
string. This is the worst class (invented edit, gate-invisible) and it breaks the §8
"never silently wrong" guarantee for these two providers.

**Fix**: gate the code-mention scan on lexical position — skip the line tail after an
unescaped `//` (Java) / `//`/`#` (PHP), and skip runs whose start byte is inside a `"…"`/`'…'`
string span (a cheap per-line quote-state pass). At minimum never rewrite inside quotes.
See open question **Q4** — the reference `lang-rust` matcher has the same theoretical
exposure for bare heads, so decide the blast radius of the fix.

---

## High

### R1 — PHP PSR-4 longest-prefix fall-through invents an edge (corrupts delete-safety)
**[REAL FLAW]** · `crates/langs/lang-fallback/src/imports.rs:265-290` (`resolve_use`)

On a prefix match whose dirs all miss on disk, the loop **falls through to the next
(shorter) prefix** instead of returning. Proven: map `{"App\\":"src/", "App\\Sub\\":"other/"}`,
with `src/Sub/Thing.php` present and `other/Thing.php` absent — `resolve_use("App\Sub\Thing")`
returns `src/Sub/Thing.php`. PSR-4 says `App\Sub\` owns that FQCN *exclusively*; with its dir
empty the class is unresolved. The resolver instead lands on a coincidental shadow file — an
invented edge that inverts delete-safety (deleting the real `other/Thing.php` looks safe when
it isn't). Shared verbatim with the move model (`lang-php/src/movefix.rs:88,106` call the same
`resolve_use`), so it also corrupts move rewrites and deletion diagnostics.

**Fix**: once a prefix matches (`rest` is `Some`), that prefix owns the FQCN — try its dirs,
then **return `None`**; never continue to shorter prefixes.

### G4 — Swift gate is silent on files outside the compiled target → un-gated edit reports "clean"
**[REAL FLAW]** · `crates/langs/lang-swift/src/gate.rs:27-63`

`swift build` compiles the SwiftPM target graph. A `.swift` file that is not part of any
target (outside every `Sources/<target>` root, in an `exclude:`d dir, or in a package with no
covering target) is simply **not compiled**. Verified: an orphan `Scratch/Orphan.swift` with
`let x: Int = "…"` builds `EXIT=0`, the type error invisible. The gate returns clean and the
reply still claims a `gated()==true` type-checked commit — a §6 honesty violation (strong
claim over an unverified edit). The provider never checks "did the compiler actually see this
file?"

**Fix**: after build, verify each edited rel appears among the compiled units (compile-log
presence, or a membership check against the resolved target sources); if an edited file was
not compiled, reject / downgrade the claim to "not in a SwiftPM target — not type-verified."

### G5 — Swift diagnostics not deduped × count-based baseline diff → false-REJECT of pre-existing errors
**[REAL FLAW]** · `crates/langs/lang-swift/src/gate.rs:103-134` (no dedup) × `ci-edit/src/lib.rs:706-732` (count-per-key diff)

`swift build` prints the same diagnostic once per build phase — the agent observed the same
`error:` **3×** (emit-module + compile phases). The spine counts occurrences per
`file:code:message` key and flags every after-instance beyond the baseline count. Because the
mirror is a fresh tempdir each call, phase multiplicity is not guaranteed identical between
the baseline build and the after build → a pre-existing error emitted 3× after but 2× at
baseline yields spurious "new" instances → **false reject of an untouched edit**. The Java
sidecar and Rust gate don't hit this (structured/deduped at source).

**Fix**: dedup `parse_swift_diagnostics` output by `(file, line, message)` before returning,
so each real diagnostic contributes exactly one instance to the count-diff.

### B1 — Java resident sidecar wedges the provider for the session; its error promises a respawn that never happens
**[REAL FLAW]** · `crates/langs/lang-java/src/gate.rs:83-85` × `ci-edit/src/composed.rs:168-172`

If the resident `GateSidecar.java` child dies mid-session (OOM-kill, JVM crash, `System.exit`,
sleep/resume breaking the pipe), `JavacSidecar::diagnostics` returns
`Err("… sidecar exited (EOF) — restart the edit to respawn it")`. But `Composed` caches the
built engine in `Arc<Mutex<Option<Box<dyn GateEngine>>>>` and only rebuilds when
`guard.is_none()`; **no path ever resets the guard to `None` on a diagnostics error**. So
every subsequent edit calls `diagnostics()` on the same dead child (`BrokenPipe`/EOF) and
returns the identical error **forever**. The user-facing message literally says "restart the
edit to respawn it" — but restarting the edit does not respawn it. PHP and Swift self-heal
(fresh `Command::output()` per call); Java's resident design is the only one missing the
reset. Fails honest (surfaces an error) but permanently disables Java writes for the session.

**Fix**: in `Composed::apply_edits`, drop the cached engine (`*guard = None`) when the gate
returns `Err`, so the next edit rebuilds; OR give `JavacSidecar` a health-check that detects
EOF (`child.try_wait()`), re-runs `start()`, and retries once. Back the promised respawn with
real respawn logic.

### M2 — PHP `use \App\Foo;` (legal leading `\`) → lost rewrite AND lost deletion diagnostic
**[REAL FLAW]** · `crates/langs/lang-php/src/movefix.rs:83,93`

The `use` branch extracts `use_fqcn` with `.trim_start_matches('\\')` (→ `App\Foo`) but then
calls `fqcn_spans(line, fqcn)` against the **raw** line `use \App\Foo;`. In `fqcn_spans` the
char before `App` is `\` (a `before_ext` char) → `before_ok = false` → zero spans → **no
`RefOccurrence` is produced at all**. So the importer's `use` is not retargeted on a move,
and `deleted_reference_diags` never flags it when the target is deleted. A leading `\` in a
`use` is valid PHP. Under-match on both capabilities.

**Fix**: the `use` occurrence is single-FQCN — don't reuse the boundary-sensitive
`fqcn_spans` against the raw line; locate the trimmed `fqcn` at its known offset (past the
`use `/`\` prefix) and emit that one span directly, mirroring the code-mention path's
re-anchor in `backslash_runs`.

### M3 — Default-package / global-namespace move silently declines the whole move (no InsertAt-to-add)
**[REAL FLAW]** · `crates/langs/lang-java/src/movefix.rs:133`, `crates/langs/lang-php/src/movefix.rs:138`

`membership_edits` can *remove* or *rewrite* a decl but has no path to *add* one where none
exists. When `from` has no `package`/`namespace` line, `package_decl_line(&content)?` /
`namespace_decl_line(&content)?` returns `None`, which propagates via `?` → `membership_edits`
returns `None` → `move_workspace_edit` returns `None` → **the engine declines the whole move,
importers included**. Moving `A.java` (default package) into `com/x/` should rewrite importers
and insert `package com.x;`; instead the model declines. For a language whose engine
(jdtls/phpactor) is often absent, this leaves no fallback for that shape. Fails safe (declines,
not corrupts) but breaks the "complete one-call moves without the engine" claim.

**Fix**: when the decl line is `None` and the destination package/namespace is non-empty, emit
`MembershipEdit::InsertAt` on `from` adding the decl at the right anchor (after `<?php` for
PHP; line 0 or after a leading license comment for Java). The vocabulary already exists.

### M4 — PHP inline `<?php namespace App;` invisible → cross-namespace move declines; same blind spot in the resolver
**[REAL FLAW]** · `crates/langs/lang-php/src/movefix.rs:165` (`namespace_decl_line`), `imports.rs:334` (`namespace_of`)

`namespace_decl_line` requires `trim_start().starts_with("namespace ")`; verified
`namespace_decl_line("<?php namespace App;\n…")` → `None` because the line starts with
`<?php`. This is legal PHP → cross-namespace `membership_edits` returns `None` → whole move
declines (same consequence as M3). The sibling resolver `php::namespace_of` (imports.rs:334)
has the **same** blind spot, so the import graph misreads this file's namespace too.

**Fix**: strip a leading `<?php` token before the `starts_with("namespace ")` test (or scan
`namespace ` as a token, not only at line start), and fix `namespace_of` in the same pass —
one resolver, per §7 (see D1).

### T1 — PHP has no real-gate e2e for baseline-diff or atomicity (soundness clauses untested)
**[REAL FLAW]** · `crates/langs/lang-php/src/lib.rs:308-344`

PHP's only `#[ignore]` diagnostic e2e is `phpstan_gate_rejects_type_error_and_accepts_clean`.
Java (`lib.rs:280-448`) and Swift (`lib.rs:270-388`) each additionally carry
`*_gate_batch_is_atomic` and `*_gate_baseline_excuses_preexisting_breakage`. §5 baseline-diff
and batch-atomicity are the clauses most likely to regress silently, and they have **zero**
real-gate coverage for PHP. The fast-tier mock cannot exercise baseline-diff (it flags on a
marker string; it has no notion of pre-existing type state). (`conformance_php_reverse_radius_reject`
does cover reverse-radius — that clause has one e2e.)

**Fix**: add `phpstan_gate_batch_is_atomic` and `phpstan_gate_baseline_excuses_preexisting_breakage`
`#[ignore]` e2e mirroring Java's, with a `: int`-returns-string pre-existing break. They skip
loudly where phpstan is absent but pin the contract where it's present.

### T2 — Java has ZERO move test (fast or e2e) — §8 committed-move entirely uncovered
**[REAL FLAW]** · `crates/langs/lang-java/src/lib.rs` (no `MoveFile`), `movefix.rs` (JSON-shape only)

Grep for `MoveFile`/`move_` in `lang-java` returns nothing. `movefix.rs` tests only assert the
WorkspaceEdit **JSON string** — the exact prior-catch pattern (the Swift "committed-move-compiles"
that was really a JSON-shape test). The behavioral guarantee — a committed cross-package move
leaves a compiling project with references and the `package` line rewritten — has no test.
Rust pins it (`committed_move_must_leave_a_compiling_crate`, `move_after_manual_decl_edit_must_not_false_clean`);
Swift now pins its degenerate form; Java — the provider whose move actually rewrites imports
**and** the package line — pins nothing end-to-end.

**Fix**: add a Java `#[ignore]` e2e: cross-package `EditOp::MoveFile` through `JavaProvider::new`
+ real javac; assert commit, package-line rewrite, importer retarget, disk move, still
compiles. jdtls absent → it exercises the movefix-hook fallback, which is what needs coverage.

---

## Medium

### R2 — Java flat-layout root resolves imports by path coincidence, no package check → invented edges
**[REAL FLAW]** · `crates/langs/lang-fallback/src/imports.rs:370` (unconditional `PathBuf::new()` root), `resolve_import:436-446`

`java_source_roots` always appends the repo root, and resolution never verifies the target's
`package` matches the import. Proven: flat repo, `App.java` with `import util.Helper;`, and a
top-level `util/Helper.java` declaring `package wrongpkg;` → edge `App.java → util/Helper.java`,
an invented edge (different package). On-demand `import config.*;` globs every `.java` in a
`config/` dir regardless of package. Maven layout makes coincidence unlikely; the flat-root
fallback makes any dotted import mirroring a directory tree a candidate.

**Fix**: when resolving via the flat/non-package-derived root, verify the target's
`package_of(content)` equals the import's package prefix before emitting the edge
(`package_of` exists at `imports.rs:451`). Or drop the unconditional root and accept more
honest misses.

### S1 — Gating executes repo-controlled build code; undocumented, no opt-out
**[DECISION NEEDED]** · `lang-swift/src/gate.rs:45-52,160-171`; `lang-java/src/gate.rs:147-174,203-209`

Security sweep found **no OUR-code command injection and no path traversal** (all `Command`
args are argv, never `sh -c`; the classpath is a single joined `-classpath` value; PHPStan
target paths are absolute tempdir paths so a `-foo.php` filename can't smuggle a flag; move/
create/delete paths are validated by `ensure_within_root` upstream in `ci-edit/src/lib.rs:522`;
tempdirs are `tempfile` RAII, no predictable `/tmp` names; the `include_str!` sidecar can't be
influenced by the repo). The one material surface is inherent and acknowledged only in code
comments: **gating a Swift/Maven/Gradle/PHP repo executes repo-controlled code** — `swift
build` runs `Package.swift` + plugins, `mvn`/`gradle` run build scripts, PHPStan bootstraps
autoloaders. `prime_index` is the sharpest instance (runs `swift build` at the **real repo
root**, not the mirror, on the first rename). `derive_paths` runs `mvn`/`gradle` at
engine-**construction** on any repo that merely contains a build file with the tool on PATH.
There is no threat-model section in the spec/architecture and no opt-out env var.

**Decision**: (a) add an explicit threat-model/untrusted-repo section to the rollout spec;
(b) provide `CI_GATE_NO_BUILD=1` that degrades Swift to `swiftc -typecheck` (accepting the
documented SIL-soundness loss) or disables the build gate honestly, and skips `mvn`/`gradle`
derivation (which already has an honest flat-classpath fallback); (c) consider making
`prime_index`'s root build opt-in.

### G6 — Java flat-classpath degrade can mask introduced errors while still claiming `gated()`
**[DECISION NEEDED]** · `crates/langs/lang-java/src/gate.rs:147-174`

When `mvn`/`gradle` classpath derivation fails, the gate falls back to a flat source-root
classpath. The comment argues this is honest because dependency-typed code carries
unresolved-symbol errors in *both* baseline and after (diff-excused). True for *pre-existing*
references — but javac error-recovery suppresses downstream type-checking against an
already-erroneous symbol, so an edit that **introduces** a real type error against a
dependency API (wrong arg type to a library method) produces no *new* diagnostic → false
clean. The provider still reports `gated()==true`. The degrade is announced only via
`eprintln!`, not in the commit reply — brushing "never silently degrades to a weaker gate"
(§6).

**Decision**: surface "dependency types unresolved — edits touching library APIs are not
fully verified" in the reply, OR treat derivation failure on a project that HAS a build file
as `ProviderBuild::Unavailable` with an install hint rather than a silent weaker gate.

### B2 — Swift gate deep-copies the whole repo tree on every edit (scaling cliff)
**[REAL FLAW / DECISION NEEDED]** · `crates/langs/lang-swift/src/gate.rs:27-63` (`copy_package_tree`)

Every `diagnostics()` call (baseline + after = two per edit) creates a fresh tempdir and
copies **every** `.swift`/`Package.swift`/`Package.resolved` in the repo, then runs `swift
build` from cold (`.build` is deliberately excluded, so nothing is incremental). O(total
source size) per edit, not O(edited files); on a large SwiftPM monorepo each one-line edit
pays a full-tree copy + cold build. Not a leak (`TempDir` RAII on all paths) but a real
scaling cliff versus the memory record's "≈ 0.1s + 65ms×radius" gate-cost target. The
full-package *build* is load-bearing for soundness; the full-tree *copy* is not.

**Fix / decision**: mirror only the target's sources, or symlink the unchanged tree and
overlay just the edited buffers, or build in place against a scratch `--build-path`. At
minimum, document the scaling characteristic.

### C1 — PHP toolchain probe (PATH-only) disagrees with the gate resolver → false "disabled"
**[REAL FLAW]** · `crates/langs/lang-php/src/lib.rs:124` vs `gate.rs:24-44`

`phpstan_status()` probes `Command::new("phpstan")` (PATH only), but the resolver the engine
actually uses, `gate::phpstan_binary()`, also honors `$CI_PHPSTAN` and `vendor/bin/phpstan`
(the common Composer location). On a repo with a vendored PHPStan and no PATH `phpstan`,
`gate_missing()` → `ProviderBuild::Unavailable("install phpstan")` disables the language even
though `PhpProvider::new` would have found the binary and gated fine — §6 honesty inverted
(claims "unavailable" when available). Every other provider probes the SAME resolver it runs
(jdtls, phpactor, sourcekit); PHP's required gate tool is the one place that must not be looser.

**Fix**: `phpstan_status().found` should call `gate::phpstan_binary(root)`. This threads a
`root` into `toolchain()`/`gate_missing()` (PHP's `engine_factory(root)` already takes one).

### D1 — Dead resolver re-exports + movefix reimplements `package_of`/`namespace_of` instead of calling the shared facade (§7)
**[REAL FLAW / CONSISTENCY DRIFT]** · `crates/langs/lang-fallback/src/lib.rs:34,42`; `lang-java/src/movefix.rs:149`; `lang-php/src/movefix.rs:154`

The facade doc advertises `java::{package_of, java_source_roots}` and `php::{namespace_of,
psr4_map}` as the shared source of truth "so the move model speaks the SAME resolver … no
divergent reimplementation." But `package_of`, `java_source_roots`, `namespace_of` have **zero
callers through the facade**, and the move models roll their **own** `fqn_package`
(`movefix.rs:149`) / `fqcn_namespace` (`movefix.rs:154`) — the doc comments even say "mirroring
the resolver's `package_of`/`namespace_of`", i.e. they duplicate the thing the facade exports
for exactly this purpose. Crate-root `pub use` escapes dead-code linting, so this dead surface
is invisible to the compiler. §7 explicitly warns duplicated logic is where the byte/char bugs
came from — and M4 is precisely a blind spot that exists in `namespace_of` **and** its
`namespace_decl_line` twin.

**Fix**: make the movefix models call the facade functions (return the index alongside the
name), collapsing `fqn_package`/`fqcn_namespace`/`package_decl_line`/`namespace_decl_line` onto
`package_of`/`namespace_of`. Fix M4's blind spot once, in the shared scanner. Drop any re-export
that stays genuinely unused.

### T3 — Deleted-reference diagnostics wired into Java & PHP `diagnostics()` but never tested at provider level
**[REAL FLAW]** · `lang-php/src/gate.rs:150-158`, `lang-java/src/gate.rs:254-261`

Both engines call `deleted_path_references` → `ci_edit::moves::deleted_reference_diags(&<Lang>MoveModel, files)`
inside `diagnostics`. The generic algorithm is tested once centrally with a `ToyModel`
(`ci-edit/src/moves.rs:404`), but no test drives a batch that DELETES a `.php`/`.java` whose
surviving importer still references it and asserts the anchored diagnostic through the REAL
provider hooks. A resolver bug (wrong PSR-4 inversion — see R1 — or a `fqcn_spans` boundary
miss — see M2) would ship a false-clean delete and every current test would still pass.
(Swift's is a genuine no-op by design — module-level imports — so Swift needs none.)

**Fix**: one fast-tier test per Java/PHP using the no-op-verdict `MoveEngine` pattern PHP
already has at `lib.rs:238`: stage a batch deleting `Helper.php` while `Consumer.php` keeps
`use App\Helper;`; assert `apply_edits` rejects naming the stranded `use`. No toolchain needed.

---

## Low

### R3 — Java `import static a.b.C.member` misses the real edge
**[REAL FLAW (miss)]** · `crates/langs/lang-fallback/src/imports.rs:95-97`

`import static lib.Util.helper;` is treated as a type FQN `lib.Util.helper` → tries
`lib/Util/helper.java` → miss → no edge, though `lib/Util.java` exists. Honest (never
invents) but weakens the graph for static-import-heavy Java. **Fix**: when a `static` child is
present and non-wildcard, drop the trailing member segment before resolving.

### M5 — Swift cross-target move reports a "handled" empty edit while doing zero `Package.swift` work
**[DECISION NEEDED]** · `crates/langs/lang-swift/src/movefix.rs:35`

`file_to_ref` returns the bare file **stem**, so `Sources/App/Util.swift` and
`Sources/Lib/Util.swift` both map to `"Util"` → `old_ref == new_ref` → engine returns `None`
(collapses with the within-target no-op). Worse, a cross-target move with a *different* name
gives differing refs + empty membership → the engine emits an empty-`documentChanges`
"handled" edit (pinned as success at `movefix.rs` test line 99-102) — it reports a *handled*
move while doing zero `Package.swift` work. Cross-target safety then rests **entirely** on
`swift build` catching a stranded target, which depends on the target's sources glob still
covering the new location (and on G1/G4 being fixed).

**Decision**: (a) make `SwiftMoveModel` target-aware (derive target from `Sources/<Target>/…`
or Package.swift) so a cross-target move returns `None` honestly and defers to the gate; or
(b) document explicitly that Swift cross-target safety is gate-only and the empty-edit result
is not a soundness claim.

### B3 — Unbounded subprocess stdout/stderr capture (PHP & Swift gates)
**[REAL FLAW]** · `lang-php/src/gate.rs:72-81`, `lang-swift/src/gate.rs:45-52`

`Command::output()` buffers the child's entire stdout+stderr with no cap; a chatty/adversarial
project (PHPStan with thousands of errors, macro-heavy `swift build`) can OOM the process. The
Java sidecar (single `read_line`) and the LSP reader (`Content-Length` framing) don't. **Fix**:
cap the captured output with a bounded reader, or stream-parse.

### B4 — No wall-clock timeout on `swift build` / PHPStan subprocess
**[CONSISTENCY DRIFT]** · `lang-swift/src/gate.rs:45-52,160-171`, `lang-php/src/gate.rs:72-81`

`Command::output()` blocks until the child exits, unbounded; a wedged compile (looping macro,
toolchain deadlock, network-fetching SwiftPM resolve) hangs the edit indefinitely. The
`ci-lsp` client, by contrast, deadlines every wait (30s/60s/120s). **Fix**: wrap the gate
`output()` calls with a timeout (spawn + `wait_timeout`, kill on expiry), mapping expiry to a
recoverable error.

### G7 — Swift `prime_index` swallows a failed root build → cold-index rename rejects mysteriously
**[CONSISTENCY DRIFT]** · `crates/langs/lang-swift/src/gate.rs:160-171,205-222`

`prime_index` runs `swift build` with `.status()` ignored and `_pollIndex` ignored. If the
root build fails (common mid-edit), the IndexStoreDB stays empty and `rename` returns only the
definition. The `swift build` gate then *rejects* a rename that silently dropped its cross-file
references — presenting as a mysterious "introduced error" rather than "index cold." **Fix**:
distinguish "index cold, rename incomplete" in the reject message (low-priority polish).

### G8 — Swift `split_diag_head` splits on the first `": "` → drops a diagnostic whose path contains `": "`
**[REAL FLAW]** · `crates/langs/lang-swift/src/gate.rs:140-154`

Splitting on the first `": "` assumes the path has none. A source path with a space-after-colon
(rare on macOS, possible in an oddly-named mirror tempdir) mis-splits and silently drops the
diagnostic — a silent-drop path with no fallback. **Fix**: anchor on the `:LINE:COL:` numeric
shape (rfind two numeric colon fields before the first `: severity`), or match `severity ∈
{error,warning,note}` explicitly.

### C2 — Java hand-rolls `MockChecker`/`MARKER` instead of the shared `lang_template::mock`
**[CONSISTENCY DRIFT]** · `crates/langs/lang-java/src/lib.rs:153-182`

PHP (`lib.rs:157`) and Swift (`lib.rs:174`) both use `lang_template::mock::factory()`; Java
reimplements a byte-identical mock with a divergent `JAVA_MOCK_TYPE_ERROR` marker and is the
only crate with no `lang-template` dev-dependency (`Cargo.toml:23`). Conformance already drives
Java through the shared mock (`conformance.rs:627`), so the local copy is pure redundancy that
can drift from the shared `GateEngine` contract. **Fix**: add the `lang-template` dev-dep, use
`lang_template::mock::factory()`/`MARKER`, delete the ~30 local lines. (Same item as dead-code
sweep D-F3.)

### C3 — Swift gate module doc contradicts the code on stdout-vs-stderr
**[REAL FLAW (doc)]** · `crates/langs/lang-swift/src/gate.rs:10,98-99` vs `53-61`

The module doc and `parse_swift_diagnostics` doc say diagnostics are on **stderr** (param
named `stderr`), while the code comment + code parse **stdout** (compiler diagnostics) + stderr
(SwiftPM manifest/toolchain). A maintainer trusting the docs would break parsing. **Fix**:
correct the docs to "stdout + stderr, both parsed"; rename the param `stderr` → `combined`.

### D2 — Debug `println!` left in a committed test
**[REAL FLAW]** · `crates/langs/lang-fallback/tests/hostile_imports.rs:190-193`

Two `[debug …] {…:?}` `println!`s with no assertion — hand-debugging scaffolding; also the sole
facade caller of `php::psr4_map`. **Fix**: remove them (assert on `map` if that was the intent,
else drop the call and the re-export per D1).

### D3 — Stale facade doc names a nonexistent `file_to_class`
**[CONSISTENCY DRIFT]** · `crates/langs/lang-fallback/src/lib.rs:37-40`

The `php` module doc says `file_to_class`; the function is `file_to_fqcn`. Cosmetic doc drift
from a rename. **Fix**: `s/file_to_class/file_to_fqcn/`.

### D4 — `SwiftMoveModel` dead `&Path` field behind `#[allow(dead_code)]`
**[DECISION NEEDED / OVERLAP-BY-DESIGN]** · `crates/langs/lang-swift/src/movefix.rs:29-30`

The `&Path` field is never read (within-target hooks are rootless); the `#[allow(dead_code)]`
+ comment justify it as a placeholder for the future cross-target `Package.swift` rewrite
(tied to M5). **Decision**: keep with the comment if cross-target work is imminent; else make
it a unit struct and drop the unused `root` param. Recommend keep.

---

## Examined and cleared

Aspects checked and found sound (not exhaustive — the notable ones):

- **Security — injection & traversal: clean.** All external commands are argv-built, never
  `sh -c`; classpath is a single opaque `-classpath` value (no per-jar arg-smuggling); PHPStan
  targets are absolute tempdir paths (a `-foo.php` filename can't become a flag); every op path
  is validated by `ensure_within_root` (`ci-edit/src/lib.rs:522`) before any write; tempdirs
  are `tempfile` RAII with randomized names (no `/tmp` TOCTOU); the `include_str!` sidecar
  source is compile-time fixed; `ignore::WalkBuilder` doesn't follow symlinks (mirror copy can't
  escape). Binary selection is PATH-based, identical to the `lang-rust`/`lang-ts` reference.
- **§2 read softness: no reachable panic.** An adversarial probe suite (empty / BOM-only /
  multibyte-unicode / truncated / 20k-brace wall / 4 MB single line / invalid-UTF-8-on-disk /
  PHP 8.5 `|>` / PHP 8.4 hooks) through `structure`/`import_graph`/`outline` for all three
  languages returned `Ok`, zero panics. PHP 8.5 pipe on tree-sitter-php 0.24.2 yields ERROR
  nodes (soft), not a crash; Swift/PHP grammar-ABI load tests pass; `split_diag_head` byte math
  and the `fqn_spans`/`backslash_runs` cursors stay on char boundaries.
- **Token-boundedness (matchers): fine.** `dotted_runs`/`backslash_runs` are single O(line)
  scans; the longest-prefix loop is O(depth) and `break`s at the first resolving prefix; output
  is bounded by real references, no duplicate spans; `splice_spans` is right-to-left and
  bound-safe.
- **Matcher boundary rules (non-string cases): correct.** Java trailing `.` accepted (member
  access / nested type), PHP trailing `\` rejected (deeper namespace) — the prior fixed
  trailing-`.` bug has **not** recurred; `@com.x.A`, `com.x.A<T>`, `new com.x.A()`,
  `App\Foo::class`, `new \App\Foo()` all match; `com.x.Abc`, `org.x.A`, `App\FooBar`,
  `My\App\Foo`, `App\Foo\Bar` all correctly rejected. (The gap is only strings/comments — M1 —
  and the leading-`\` *use* line — M2.)
- **Swift import graph: honestly empty.** Swift never reaches a resolver arm; module-level
  imports produce no invented file→file edges (validates the §3 empty-graph case).
- **PHP resolver happy paths: sound.** Longest-prefix WIN (present file), grouped `use A\{B,C}`,
  alias `as`, leading-`\` trim, empty-prefix `"": "src/"`, no-composer / malformed-composer /
  PSR-0-only all yield the honest result with no panic. (The one hole is the miss-fall-through —
  R1.)
- **Java severity mapping: sound.** The sidecar filters `kind == "ERROR"` from a
  `DiagnosticCollector` (structured, not text-parsed); JRE-not-JDK, sidecar EOF, and unparseable
  replies all map to `Err` (reject), never silent clean. (The resident-death *wedge* is B1; the
  classpath *degrade* is G6.)
- **PHP unparseable-JSON → `Err`.** A text fatal on *stdout* is surfaced (`gate.rs:97-102`) —
  the hole is *empty* stdout + top-level `errors` (G2/G3), not this path.
- **`gated()` honesty at construction: correct.** All three go through
  `ProviderBuild::Unavailable` + install hint when the required tool is missing; the honesty
  gaps are narrower (a present tool that no-ops on a specific input: G1/G2/G4).
- **Determinism & path normalization.** Graph output is `BTreeMap`/sorted+deduped; all emitters
  route through `norm()` to clean repo-relative keys; no HashMap-iteration leak.
- **Structural spine consistency (§7): strong.** `toolchain()`/`gate_missing()` shapes,
  `Composed` assembly (assembly-only lib.rs, no hand-wired channels), the three 29-line
  `sidecar.rs`, `JavacSidecar::Drop` (kill+wait), error types (`Error::Driver` throughout),
  module layout, naming (`<Lang>Provider/Engine/MoveModel`), and registry wiring
  (`LangSpec`/`Lang`/`make_provider`/`FbLang` rows) are symmetric across the three. No orphan
  files, no unwired mods, no spine reimplementations (`byte_offset`/`reverse_import_map`/
  `transitive_reverse_imports`/`commit_edits` all delegated). `cargo build --workspace` is
  0-warning.
- **`will_rename` ordering divergence: by design.** Java/PHP try engine-native `willRenameFiles`
  first (jdtls/phpactor implement it), Swift goes straight to hooks (sourcekit refutes it) —
  documented at each site, correct application of "engine-native where it exists."
- **Matcher-duplication across the three crates: NOT a §7 violation.** `fqn_spans` vs
  `fqcn_spans` vs Rust's `bare_head_spans` encode genuinely different boundary rules (`.` vs `\`
  vs `::`, trailing-`.` accept vs trailing-`\` reject); Swift correctly has none. Forcing one
  parameterized helper would be a false abstraction. (The duplication that *should* collapse is
  the decl-line/package-inversion helpers — D1.)
- **Conformance instance shape.** Each language has an ungated-fallback + `*_gated_mock`
  (read+edit battery) + a real-tool `#[ignore]` instance, matching the TS/Rust shape; PHP has an
  extra `reverse_radius_reject`. No provider is in the read battery but missing from edit. The
  gaps are in the provider-crate e2e tier (T1/T2/T3), not the conformance layer.
- **The prior Swift catch is fixed for real.** `committed_within_target_move_compiles_with_no_rewrites`
  drives a real `EditOp::MoveFile` through `swift build` and asserts disk state — ran and passed
  (6 ignored Swift e2e green in 157s; 8 ignored Java e2e green).

---

## Open questions for review

1. **G1/G2/G3 shared fix** — extract one `reject_if_tool_failed(status, stdout, stderr, diags)`
   helper in `ci-edit` and apply it in the Swift and PHP gates (recommended), plus the two
   conformance fixtures? This is the top-priority cluster.
2. **S1 threat model** — add a threat-model section + a `CI_GATE_NO_BUILD` opt-out, and make
   `prime_index`'s root build opt-in? Or accept build-execution as an operator-trust assumption
   documented in the spec only?
3. **G6 Java degrade** — on a project that HAS a build file but derivation fails: reject as
   `Unavailable` (strict, honest) or run the flat-classpath gate but surface a weak-claim
   warning in the reply?
4. **M1 blast radius** — the string/comment over-match fix: cheap quote/comment skip on the
   three new providers only, or also close the reference `lang-rust` matcher's theoretical
   exposure in the same pass? (The reference is the §8 semantics oracle — do we tighten it too?)
5. **M5 / D4 Swift cross-target** — make `SwiftMoveModel` target-aware now (returns `None`
   honestly for cross-target), or document gate-only cross-target safety and keep the degenerate
   model + its `#[allow(dead_code)]` field until the cross-target phase?
6. **B2 Swift copy** — bound the mirror now (target-only / symlink+overlay / scratch build-path)
   or ship with a documented scaling caveat and defer?
7. **Fix sequencing** — proposed order: **G1/G2/G3** (false-clean, critical) → **M1** (invented
   edits) → **R1** (invented edge) → **B1, G4, G5** (high gate/robustness) → **M2/M3/M4 + D1**
   (PHP/Java move + the shared-resolver collapse, done together since they touch the same
   scanners) → **T1/T2/T3** (close the test gaps that would have caught the above) → medium/low.
   Each item: impl → independent adversarial verify → fix-once-or-abort.
