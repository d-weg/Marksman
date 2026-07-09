# File-surface consolidation — spec for review

**Status: DRAFT — nothing in this document is implemented. It is a design spec for review.**

Third audit in the series (op surface → test surface → file surface). The question that
prompted it: *do we have too many files?* The measured answer, stated up front: **no —
the file and crate counts are the architecture, not sprawl.** 61 Rust files carrying
~20.7k lines across 21 crates (~340 lines/file); the real findings run in the *opposite*
direction (two files still too big) plus documentation rot. Three sweeps fed this spec:
crate granularity (full dependency graph + keep-or-merge verdicts), in-crate file
organization (monoliths, orphans, dead code), and the non-Rust surface (docs, scripts,
root files).

## 1. The verdict on "too many files"

- **Crates: all 21 earn their boundaries — zero merges recommended.** The dependency
  graph is acyclic with sensible hubs (entry binaries `ci-cli`/`ci-mcp` at in-degree
  10/11; everything else low-degree). Every small crate carries a real seam:
  `ci-embed` isolates the ML deps (tokenizers/safetensors) so the build pipeline stays
  model-free; `ci-scip` isolates protobuf and serves both providers; `ci-treesitter` is
  the ABI enforcer (one tree-sitter version across three grammar crates); `ci-vfs` is
  the transaction spine, testable without the gate; `ci-walk`/`ci-arch` split discovery
  from structure introspection. One watch item: `ci-lsp-index` has a single consumer
  (lang-ts) — keep, but reconsider if a second LSP-sweep consumer never materializes
  (the Swift rollout may BE that consumer; see the language-rollout spec).
- **Files: no orphans, no dead code.** Every `.rs` on disk is reachable (mod tree or
  `[[bin]]`); the sidecars are wired (`sidecar.cjs` via `include_str!` at
  `tsmorph.rs:17`); post-refactor spot-checks found zero unreachable pub items; small
  files earn their keep (`lang-ts/outline.rs` at 28 lines is the P3 delegation pattern,
  correctly separate).
- **Large crates: cohesive.** `ci-edit` (4 modules) and `ci-core` (11 modules) checked
  for accretion — none; module boundaries match capabilities.

## 2. Findings

### F1. `ci-mcp/src/main.rs` — the last true monolith (2,316 lines, 7 capabilities)

The P11 reorg deliberately skipped it. The sweep mapped seven interleaved capabilities:
server infrastructure (:29-251), retrieval tools (:253-483), symbol resolution — the
largest and most tangled at 920 lines (:524-1444), edit orchestration (:1448-1883),
output formatting (scattered :1607-2147), JSON-RPC plumbing (:2151-2316), and dispersed
utilities. Verdict: split by capability, same pure-move discipline as P11. Note the
inventory's claim that a bin crate "requires lib.rs extraction first" is wrong — a bin
crate declares sibling modules from `main.rs` directly (`mod resolve;` etc.); no
`lib.rs` is needed unless another crate wants to import it (none does).

### F2. `lang-fallback/src/lib.rs` — second monolith (1,375 lines, 6 capabilities)

Language config (`FbLang`), provider traits, outline, structure extraction (476 lines),
import resolution (233 lines), and edit support in one file. This crate is also the
**on-ramp for every new ungated language** (the rollout ladder's step 1), so its
navigability has compounding value — the Swift/PHP/Java rollout adds rows here.
Verdict: split into `structure.rs` / `imports.rs` / `gate.rs` with `FbLang` + provider
assembly staying in `lib.rs`; pure moves, library crate, no blockers.

### F3. README advertises the deleted six-tool surface — stale, verified

`README.md:28-32` documents `retrieve_context`, `describe_architecture`,
`find_symbols`, `list_anchors`, `read_node` as standalone MCP tools. Since the facade
A/B and P7, the surface is exactly `apply_edits` + `inspect` (the five are inspect
modes), and the removed names now return unknown-tool errors — the README would walk a
new user into calls that fail. (One sweep graded this "accurate"; direct verification
overrode it.) Verdict: rewrite the tool table to the two-tool surface with the modes
documented under `inspect`.

### F4. One broken cross-doc anchor

`provider-contract.md:20` links `benchmarks.md#3-what-this-settles--the-provider-rollout-ladder`;
the section was renumbered — the content now lives at §2.5 ("What this settled — how
languages roll out"). One-line fix.

### F5. Minor script/doc hygiene

- `scripts/bench.py` requires the frozen Node prototype as oracle but says so nowhere —
  a header comment marking it a legacy comparative harness prevents a wasted afternoon.
- `scripts/multilang-bench/` is live but unexplained — one README line on its purpose
  and relation to agent-bench.
- `docs/architecture.md` test counts (already tracked as T7 in the test-surface spec —
  not duplicated here).

### Explicitly examined and cleared

Crate merges (all rejected with reasons — see §1); orphaned files (none); dead code
(none); `legacy-benchmark.md` (intentional archive, correctly labeled); `docs/eval/`
(live, gate for weight changes); bench fixtures (all five referenced by tasks.json);
`Cargo.toml` workspace members (exact match with disk); `.gitignore` (no leaked
artifacts); `marksman.config.json` (current); CONTRIBUTING.md (current).

## 3. Proposals

**P1. Split ci-mcp by capability** (F1) — sibling modules declared from `main.rs`
(`server` infra, `resolve` symbol resolution, `tools_read` retrieval, `tools_edit`
edit orchestration, `render` output formatting; JSON-RPC dispatch + `main()` stay).
Pure verbatim moves; the 10 in-file tests move with their code. Acceptance: zero
behavior change, `cargo test -p ci-mcp` green with identical test names, no other crate
touched. Size M (it's one file, but the resolution cluster is tightly coupled).

**P2. Split lang-fallback by capability** (F2) — `structure.rs`, `imports.rs`,
`gate.rs`; `FbLang` + `FallbackProvider` assembly + `outline()` stay in `lib.rs`. Pure
moves, re-exports preserve paths (lang-ts imports from this crate). Acceptance: workspace
green, conformance battery (8 ungated instances) green, no test edits. Size S-M.

**P3. README two-tool rewrite** (F3) — tool table becomes `apply_edits` + `inspect`
(modes: search/symbol/file/node/map, folding the five old rows into mode descriptions);
the §"reads stay true" prose keeps its content but names `inspect` not the dead tools.
Acceptance: no tool name in the README that `tools/list` doesn't advertise (the P7 test
defines the set). Size S.

**P4. Fix the contract anchor** (F4) — point to benchmarks.md §2.5. Size XS.

**P5. Script hygiene** (F5) — legacy header on `bench.py`; one README line for
`multilang-bench`. Size XS.

### Non-goals (explicit)

- No crate merges or new crates; no changes to the dependency graph.
- No file-count reduction drives: small files that delegate (post-P3/P4 pattern) stay.
- `legacy-benchmark.md`, `docs/eval/`, bench fixtures untouched.
- ci-mcp gets no `lib.rs` (nothing imports it; the bin-module split suffices).

## 4. Open questions for review

1. **P1 module names**: the five proposed (`server`/`resolve`/`tools_read`/`tools_edit`/
   `render`) or a different cut? The 920-line resolution cluster is the load-bearing
   decision — it stays whole in this proposal (it's cohesive), only relocated.
2. **Sequencing vs the language rollout**: P2 (lang-fallback split) should land BEFORE
   the Swift/PHP/Java work adds rows to that crate — agree to order it first?
3. Execution as before (orchestrated, item-per-item, adversarial verify, uncommitted)?

## 5. Execution order & effort (once approved)

| Step | Items | Size | Gate |
|---|---|---|---|
| 1 | P4, P5 (doc/script one-liners) | XS | doc review |
| 2 | P3 (README rewrite) | S | P7 surface test defines the tool-name set |
| 3 | P2 (lang-fallback split — before the language rollout) | S-M | workspace + conformance |
| 4 | P1 (ci-mcp split) | M | cargo test -p ci-mcp + workspace |
