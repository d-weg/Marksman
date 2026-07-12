# PHP provider — status & benchmark

> **Status: WIP (experimental).** Gated PHP support landed as part of the java/php/swift
> language rollout. Reads and the PHPStan gate are exercised by the test suite; the benchmark
> numbers below are **preliminary and not yet clean** (see [caveats](#benchmark-preliminary)).
> Not yet promoted to a supported language.

## What works

- **Reads** — in-process tree-sitter: structure/outline, import graph, retrieval. No external
  dependency.
- **Gate** — **PHPStan** (level 5, overridable via `CI_PHPSTAN_LEVEL`) type-checks each edit. The
  gate mirrors the **whole project** into an overlay and resolves sibling classes via
  `scanDirectories`, so a cross-file reference (`new DocEntry(...)`, `$doc->field`) type-checks
  correctly — the PHP analog of Java's `-sourcepath`.
- **Move** — the syntactic movefix hooks rewrite the `namespace` declaration + every referencing
  `use`/FQCN, gated by PHPStan. Works **without** an LSP.
- **Rename** — cross-file symbol rename uses **phpactor**. Without it, rename is rejected with a
  hint to reissue it as gated `replace_text` edits (PHPStan then validates the result).

## Toolchains

| tool | needed for | required? | install |
|---|---|---|---|
| `php` | running PHPStan | **required** for gated edits | `brew install php` (8.1+) |
| `phpstan` | the edit gate | **required** for gated edits | `composer require --dev phpstan/phpstan`, or the `.phar` (`$CI_PHPSTAN`) |
| `phpactor` | precise cross-file rename | optional | the PHAR from [phpactor releases](https://github.com/phpactor/phpactor/releases) (`$CI_PHPACTOR`) |

`marksman doctor <repo>` reports what a PHP repo needs and what's present.

## Known gaps

- **Rename hard-requires phpactor** — no syntactic rename fallback yet; the gated `replace_text`
  path (surfaced in the reject) is the workaround, and it works well because the PHPStan gate
  validates it. **No host phpactor? Use [container mode](../../docker/README.md)**: the
  `marksman-php` image ships phpstan AND phpactor (`docker build -f docker/marksman-php.Dockerfile
  -t marksman-php docker/`, then `CI_SANDBOX=oci`) — shipped and e2e-verified
  ([container-gate spec](../container-gate-spec.md)).

## Recent fix

The PHPStan gate previously analysed only the touched files in isolation, so sibling classes read
as **"unknown class"** — it **false-rejected correct cross-file edits** (a schema-field edit blew
one benchmark cell to ~1M tokens as the agent worked around the phantom errors) and
**false-accepted** real cross-file arity breaks. Fixed by mirroring the whole project +
`scanDirectories`; regression test `phpstan_gate_resolves_sibling_class_across_files`.

## Benchmark (preliminary)

Same corpus and tasks as the [main suite A/B](../benchmarks.md#1-does-it-help--the-suite-ab),
ported to a PHP fixture (PHPStan as the gate). Median $ per task, baseline vs Marksman; **run 0,
single pass, taken BEFORE the gate fix — do not cite.**

| task | baseline $ | Marksman $ | Δ$ | note |
|---|--:|--:|--:|---|
| rename | 0.064 | 0.062 | −4% | recovered via gated replace_text |
| move | 0.216 | 0.046 | **−79%** | movefix (clean win) |
| locate-edit | 0.108 | 0.045 | **−58%** | |
| body-edit | 0.127 | 0.093 | −27% | reply carried phantom "pre-existing" errors (now fixed) |
| add-symbol | 0.137 | 0.050 | **−63%** | |
| schema-field | 0.154 | 0.755 | **+389%** | 🔴 gate false-reject bug — **now fixed**, re-run pending |
| type-rename | 0.240 | 0.228 | −5% | phpactor absent → replace_text |

**Why "do not cite":** `schema-field` hit the isolated-overlay gate bug (now fixed) and every
Marksman cell's reply carried phantom "pre-existing" errors from the same cause. Re-run all PHP
cells after the fix — `schema-field` should collapse from ~1M tokens to a clean win.
