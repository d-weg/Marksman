# Producer-surface consolidation — spec for review

**Status: EXECUTED (2026-07-11, branch `consistency-audit`) — P1–P3 landed, verified green.**
P1: `TSGO_VERSION = "7.0.0-dev.20260707.2"` pins both npx tsgo paths (the forced tier and the
lsp-sweep launcher); local `CI_TSGO`/PATH untouched. P2: `derive_paths(root, sandbox)` — the
mvn/gradle presence probe (`tool_available`) and both derivations run through `tool_command` +
`sandbox.run_capped`; host behavior identical, container mode now probes/derives in the image.
P3: contract §10 (producers), the §5 deletion-convention clause, and the §3 honest-empty-graph
sentence added. Verification: `lang-ts` + `lang-java` `#[ignore]` tiers 17/17 (incl. the OCI
java e2e and the scip/tsgo/ts-morph paths), workspace 257/257, clippy 0 across all targets.
LEAVEs recorded below stand.

The per-language inconsistency audit (follow-up to `implementation-consolidation-spec.md`,
whose scope was the gate/edit surface) inventoried the READ-ARTIFACT side: what produces each
language's semantic data, how it is invoked, pinned, cached, and refreshed. The headline,
stated up front: **the consumption layer is already uniform** (every producer's output is
normalized into the one internal model; both scip users share `ci_core::fingerprint`; all
artifacts live under `.marksman/`) — the inconsistencies are concentrated at the producer
*invocation* seam, and most of the discipline that exists (pins, fingerprint participation,
npx serialization) is folklore encoded in two crates rather than contract. This spec pins the
one unpinned tool, fixes the one environment-blind derivation, and writes the discipline down
as contract clauses — the cheap alternative to the "build our own producers" option, which
was measured and rejected (LSP-sweep parity at 38x the cost; see `benchmarks.md`).

## Terms

- **Producer** — an external tool that emits a read artifact consumed at open time:
  `scip-typescript` (TS), `rust-analyzer scip` (Rust). Distinct from a **gate tool** (verdict
  at edit time) and a **rename engine** (LSP) — those are contract §5/§9 territory.
- **Drift class** — an unpinned tool resolving to a different version across cache
  refreshes, changing behavior with no code change (the tsls/typescript-7 false-clean was
  this class; fixed in `5933d1c`).
- **§N** — clause N of `docs/provider-contract.md`.

## 1. Current state

| | producer | pinned | fingerprint | artifact |
|---|---|---|---|---|
| TS | `npx @sourcegraph/scip-typescript@0.4.0` | ✅ (version participates in the fingerprint) | shared `ci_core::fingerprint` | `.marksman/index.scip` |
| TS (lsp-sweep arm) | tsgo sweep via ci-lsp-index | n/a | **none — always re-sweeps** | `.marksman/index.lspx.scip` |
| Rust | `rust-analyzer scip` | host binary (unconditional-trust env) | shared, drift-seeded partial refresh | `.marksman/rust.scip` |
| java/php/swift/fallback | none (tree-sitter live) | n/a | n/a | none |

Gate-tier tooling pins (context): ts-morph pinned, tsls+typescript pinned (test and
production), **tsgo via npx UNPINNED** (`@typescript/native-preview`, no version).

## 2. Findings

Verdicts as in the sibling specs: **real drift risk** (fix), **gap** (add), **overlap by
design** (keep, document), **decision needed**.

### F1. tsgo fetched unpinned via npx — real drift risk

`engine.rs` pins `SCIP_TS_VERSION`, `TSMORPH_VERSION`, and (since `5933d1c`) the tsls pair —
but `tsgo_lsp_command()` and the forced `CI_EDIT_ENGINE=tsgo` tier fetch bare
`@typescript/native-preview`. That package publishes **dated dev builds** on a moving stream;
it is the exact class that broke tsls, on the *default-preferred* gate tier's npx path. The
auto tier is exposure-limited (it only uses a LOCAL tsgo), but the forced tier and the
`CI_TS_MODE=lsp` sweep both ride the unpinned fetch. `CI_TSGO`/PATH resolution stays
unconditional-trust (settled semantics).

### F2. Java classpath derivation is environment-blind — real drift risk

`derive_paths` probes **host** mvn/gradle (`tool_present`) and runs them on the **host**,
even under `CI_SANDBOX=oci` — the last gate-adjacent subprocess not routed through the
sandbox (flagged in implementation-consolidation P3 as out of scope). Consequence: in
container mode the classpath derives from whatever the *host* happens to have — a different
toolchain than the verdict runs on, §9's exact bug class (the verdict itself is honest: a
wrong classpath degrades to baseline-excused errors, never a false clean — which is why this
is a drift risk, not a soundness flaw).

### F3. The producer discipline is folklore, not contract — gap

What makes TS/Rust producers well-behaved today — pinned version **participating in the
fingerprint** (a tool bump = a reindex, never a stale artifact served under a new tool),
artifacts under `.marksman/`, staleness via `ci_core::fingerprint`, npx staging serialized
against concurrent instances, producer stdout never touching the protocol stream — is
implemented convention. Nothing obliges the next producer to any of it. The contract has
gate-tool clauses (§9) but no producer clauses.

### F4. The deletion convention is implemented four ways and documented nowhere — gap

"Empty buffer = deletion stand-in" is honored by four different materialization strategies
(rust: not re-created during in-place staging; php: withheld from the mirror; swift: removed
from the mirror; java: kept as an empty valid unit by the sidecar). Each is forced by its
tool's constraints — the *strategies* are overlap by design — but the convention they all
implement is stated only in scattered comments. A new gate's author has nothing normative to
read.

### Explicitly examined and cleared

- **lsp-sweep arm unfingerprinted** — LEAVE. It is a measured comparison arm (38x slower at
  scale; scip-typescript stays the producer), not a product path; investing cache machinery
  in a settled A/B's loser contradicts the research-branch policy. Recorded here so the next
  audit doesn't re-flag it.
- **TS reads needing Node** — intrinsic to the scip-typescript choice; the container story
  for TS (deferred M6) is the fix, not a producer rewrite.
- **Refresh semantics differ (TS full rebuild vs Rust drift-seeded)** — overlap by design:
  rust's graph is separable per file, scip-typescript's artifact is not.
- **Tool-crash `Diag` sentinel files differ** (`Cargo.toml`/`phpstan`/`Package.swift`) —
  cosmetic; the anchor is per-language on purpose (the file a user would open).
- **Edge-less fallback languages (Go/Ruby/C/C++)** — their empty import graph (weaker gate
  radius) is the ungated tier's documented honesty, but §3 doesn't SAY the graph may be
  legitimately empty per language; one sentence added under P3, no code.

## 3. Proposals

**P1. Pin tsgo** (addresses F1) — `TSGO_VERSION` const beside the other pins; both npx paths
(`tsgo_lsp_command`, the forced tier) fetch `@typescript/native-preview@{TSGO_VERSION}`.
Dev-stream package: the pin is a dated build, bumped deliberately.
- Acceptance: `lang-ts` fast tests green; the `#[ignore]` tsgo-tier e2es green.
- Risk: low. Local-tsgo (`CI_TSGO`/PATH) untouched.

**P2. Java classpath derivation runs where the gate runs** (addresses F2) —
`derive_paths(root, sandbox)`: the mvn/gradle presence probe and the derivation commands go
through `tool_command` + `sandbox.run_capped` (containerized: the image's PATH answers; host:
behavior-identical to today). A container image without mvn/gradle degrades exactly like a
host without them — the documented honest tier.
- Acceptance: `lang-java` fast green (the flat-classpath unit test passes with a
  `HostSandbox`); javac `#[ignore]` tier green; OCI java e2e green.
- Risk: low. The verdict path is untouched; only derivation moves environments.

**P3. Contract clauses** (addresses F3, F4) — new provider-contract **§10. Read-artifact
producers**: pinned version participating in the source fingerprint; artifact + fingerprint
under `.marksman/` via `ci_core::fingerprint`; concurrent-staging serialization; stdout
hygiene; "adopt a producer only at the ladder's maturity step". Plus one clause in §5 naming
the **deletion convention** (empty buffer = deletion stand-in; every materialization strategy
must make the deleted file *absent to the checker* — the four current spellings listed), and
one sentence in §3 (an import graph may be honestly empty per language; empty ⇒ the gate's
radius is the edited files, which the tier's reply wording must not overclaim).
- Acceptance: the clauses read true against the code they describe (they document what P1/P2
  just made uniformly true).

### Non-goals (explicit)

- No in-house semantic producers (measured: LSP-sweep 38x; own resolvers = re-implementing
  compilers). No new artifact format — SCIP normalized by `ci-scip` IS the neutral format.
- No fingerprint for the lsp-sweep arm (cleared above). No TS container work (M6, separate).
- No change to rust/ts unconditional-trust env semantics.

## 4. Open questions for review

1. Should producers eventually run inside the language image (making the pin an image
   property, like the gates)? (Rec: yes, but as part of TS-container M6 — not now.)

## 5. Execution order & effort (once approved)

| Step | Items | Size | Gate |
|---|---|---|---|
| 1 | P1 tsgo pin | XS | lang-ts fast + tsgo `#[ignore]` e2es |
| 2 | P2 java derivation env-aware | S | lang-java fast + `#[ignore]` + OCI e2e |
| 3 | P3 contract clauses | XS | clauses spot-check true |
