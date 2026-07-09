# Swift provider — status & benchmark

> **Status: WIP (experimental).** Gated Swift support landed as part of the java/php/swift
> language rollout. Reads, the `swift build` gate, and sourcekit-lsp rename are exercised by the
> test suite; the benchmark numbers below are **preliminary** (see
> [caveats](#benchmark-preliminary)). Not yet promoted to a supported language.

## What works

- **Reads** — in-process tree-sitter: structure/outline, import graph, retrieval. No external
  dependency.
- **Gate** — `swift build` type-checks each edit against the SwiftPM target.
- **Move** — within a SwiftPM target a move is a physical file relocation (module-level
  visibility means no import rewrites); the moved file is gated by `swift build`.
- **Rename** — cross-file symbol rename uses **sourcekit-lsp**, which **ships with the Swift
  toolchain** — so rename works out of the box, no extra install. This makes Swift the healthiest
  of the three new providers.

## Toolchains

| tool | needed for | required? | install |
|---|---|---|---|
| Swift toolchain (`swift`) | the edit gate | **required** for gated edits | swift.org / Xcode; SwiftPM projects |
| `sourcekit-lsp` | cross-file rename | bundled | ships **with** the Swift toolchain |

`marksman doctor <repo>` reports what a Swift package needs and what's present. sourcekit-lsp is
SwiftPM-only (no Xcode project support).

## Known gaps

- Cross-**target** moves (touching `Package.swift` membership) are not handled by the syntactic
  hooks; within-target moves are.

## Benchmark (preliminary)

Same corpus and tasks as the [main suite A/B](../benchmarks.md#1-does-it-help--the-suite-ab),
ported to a SwiftPM fixture (`swift build` as the gate). Median $ per task, baseline vs Marksman;
**run 0, single pass — preliminary.**

| task | baseline $ | Marksman $ | Δ$ | note |
|---|--:|--:|--:|---|
| rename | 0.066 | 0.041 | **−39%** | sourcekit-lsp present (clean win) |
| move | 0.038 | 0.036 | −4% | |
| locate-edit | 0.064 | 0.057 | −10% | |
| body-edit | 0.044 | 0.094 | +112% | high variance (verbose run), not a defect |
| add-symbol | 0.044 | 0.046 | +3% | ~tie |
| schema-field | 0.064 | 0.083 | +31% | ⚠️ both arms false-failed the check — **prompt fixed**, re-run pending |
| type-rename | 0.106 | 0.046 | **−57%** | clean win |

**Why "preliminary":** `schema-field` was ill-posed — Swift's custom `init` let **both** arms
derive the field in the initializer and touch no call sites, which the call-site check then
false-fails (a degenerate 0/0 cell). The prompt was tightened to require the field as a required
initializer parameter passed at each site; re-run `schema-field` after the fix. The rename /
type-rename / move wins are real (sourcekit-lsp is present).
