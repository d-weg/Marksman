# Java provider — status & benchmark

> **Status: WIP (experimental).** Gated Java support landed as part of the java/php/swift
> language rollout. Reads and the `javac` gate are exercised by the test suite; the benchmark
> numbers below are **preliminary and not yet clean** (see [caveats](#benchmark-preliminary)).
> Not yet promoted to a supported language.

## What works

- **Reads** — in-process tree-sitter: structure/outline, import graph, retrieval. No external
  dependency.
- **Gate** — a resident `javac` sidecar (`GateSidecar.java`) type-checks each edit with the rest
  of the project on `-sourcepath`, so a change is validated against every file it could break
  (cross-file resolution is correct — a consumer of an edited class is re-checked).
- **Move** — the syntactic movefix hooks rewrite `package` declarations + importers, gated by
  `javac`. Works **without** an LSP.
- **Rename** — cross-file symbol rename uses **jdtls** (the Eclipse JDT language server). Without
  jdtls, rename is rejected with a hint to reissue it as gated `replace_text` edits (the `javac`
  gate then validates the result).

## Toolchains

| tool | needed for | required? | install |
|---|---|---|---|
| `javac` (JDK) | the edit gate | **required** for gated edits | any JDK on `PATH` |
| `jdtls` | precise cross-file rename | optional | `brew install jdtls` (needs Java 21+, e.g. `brew install openjdk@21`) |
| `mvn` / `gradle` | dependency classpath (typed 3rd-party code) | optional | build tool on `PATH` |

`peashooter doctor <repo>` reports what a Java repo needs and what's present.

## Known gaps

- **Rename hard-requires jdtls** — there is no syntactic rename fallback yet; the gated
  `replace_text` path (surfaced in the reject) is the workaround. **No host jdtls? Use
  [container mode](../../docker/README.md)**: the `peashooter-java` image ships the JDK gate AND
  jdtls, so rename works with no host install (`docker build -f docker/peashooter-java.Dockerfile
  -t peashooter-java docker/`, then `CI_SANDBOX=oci`) — shipped and e2e-verified
  ([container-gate spec](../container-gate-spec.md)).
- Symbol resolution has two papercuts that cost a round-trip: `"'DocEntry' is ambiguous (2
  definitions)"` (a class vs its same-named constructor) and `"symbol not found — pass a path"`
  for a bare qualified method name.

## Benchmark (preliminary)

Same corpus and tasks as the [main suite A/B](../benchmarks.md#1-does-it-help--the-suite-ab),
ported to a Java fixture (`javac` as the gate). Median $ per task, baseline vs Peashooter; **run
0, single pass, contaminated — do not cite.**

| task | baseline $ | Peashooter $ | Δ$ | note |
|---|--:|--:|--:|---|
| rename | 0.071 | 0.108 | +54% | jdtls absent → manual fallback |
| move | 0.132 | 0.047 | **−64%** | movefix (clean win) |
| locate-edit | 0.077 | 0.057 | −25% | |
| body-edit | 0.054 | 0.077 | +43% | "pass a path" round-trip |
| add-symbol | 0.058 | 0.049 | −16% | |
| schema-field | 0.088 | 0.169 | +92% | ambiguous-symbol + field-order friction |
| type-rename | 0.190 | 0.264 | +39% | jdtls absent → manual fallback |

**Status of this table — known cause, workaround shipped, rerun pending.** The two losing cells
(`rename`, `type-rename`) measure the **jdtls-absent** path, not the intended engine: without
jdtls, cross-file rename falls back to fully manual editing. This is a known limitation, not a
gate defect — the gated `replace_text` workaround is surfaced in the reject, and **container mode
ships jdtls** so the intended engine runs with no host install (see [Known gaps](#known-gaps)).
The `move`/`locate`/`add-symbol` wins are already real. The flagged cells stay flagged until a
rerun with jdtls present (host install or container mode) replaces the fallback numbers.
