# codeindex-rs — current state

A from-scratch Rust rewrite of the TS tool `codeindex`: local, zero-API code retrieval +
gated structural edits, **language-agnostic** via a provider seam.

## Core principle

> **The core is pure Rust and needs nothing external to run. Anything outside Rust —
> and anything language-specific — lives in the language provider, never in the core.**

- **Core (pure Rust, in-process):** `ci-core`, `ci-walk`, `ci-embed`, `ci-index`,
  `ci-retrieve`, `ci-build`, `ci-vfs`, `ci-lsp` (generic LSP *transport*), `ci-arch`,
  `ci-cli`. No subprocess, no Node, no per-language knowledge.
- **Provider (`lang-ts`) owns all external/language-specific deps:** `scip-typescript`
  (read), `typescript-language-server` (write gate) — both via `npx`, no global install —
  and the in-process `tree-sitter-typescript` grammar. Adding a language = a new provider
  crate, not a change to the core.

`ci-lsp` is a language-agnostic LSP client: the provider passes it the server `Command`;
`ci-lsp` itself knows nothing about TypeScript.

## How it works

```
Agent / CLI / MCP  (pure Rust)
        │
   ci-core  (BM25 · Model2Vec · RRF · weighting · retrieval)   ← language-blind
        │  LanguageProvider:  granularity() · structure()->Node tree · import_graph() · apply_edits()
        ▼
   lang-ts  (TypeScript provider, owns the external tooling)
     READ   ── SCIP (scip-typescript -> index.scip)  + tree-sitter (in-process)  ── merged
     WRITE  ── VFS overlay (ci-vfs) + type-check gate (ts-morph sidecar, warm; ci-lsp -> tsls fallback)
```

### Read = SCIP **+** tree-sitter (merged)
- **SCIP** gives compiler-grade semantics: symbol identity, cross-file references → the
  import graph, and precise `enclosing_range` per symbol. External, but precise.
- **tree-sitter** (in-process, no external tool) subdivides each SCIP-anchored symbol into
  **local** sub-nodes (`parameter` / `returnType` / `body`). Used ONLY for local syntax
  inside a range SCIP already pinned — never for semantics — so its precision limits don't
  bite. This makes `structure()` a **deep AST tree** → `Granularity::Ast`.
- Net: SCIP's "no AST → no sub-symbol edits" weakness is solved by tree-sitter; tree-sitter's
  "no semantics" weakness is covered by SCIP. Each fixes the other.

### Read freshness (both providers — the index never lies in-session)
- **TS**: after a committed `apply_edits`, the warm ts-morph sidecar re-describes the changed
  files (`fileInfo` op: symbols + resolved imports) into a per-file **read override** consulted
  before the loaded SCIP index — new symbols and new import edges are visible immediately, no
  reindex. Startup stays fingerprint-cached; the overlay covers the same-session window.
- **Rust**: `structure()` and the `mod` graph read disk live (always fresh). The opt-in
  `rust-analyzer scip` use-graph is fingerprinted at `refresh_scip` time; at load, files that
  drifted since (and files committed in-session) get their edges recomputed from tree-sitter
  (`mod` + resolved `use` paths) as an overlay — scip fidelity for unchanged files, syntax
  fidelity for changed ones. A cache with **no** fingerprint is refused (mod-graph fallback),
  never served at unknown age.

### Write = VFS transaction + LSP gate
- Edits stage into an in-memory **VFS** overlay (`ci-vfs`); disk untouched until commit.
- The **gate**: push `didChange` with the overlay to the language server (`ci-lsp`),
  collect in-memory diagnostics, **baseline-diff** (fail only on NEWLY introduced errors),
  then commit-or-roll-back atomically. `ci-lsp` prefers **LSP 3.17 pull diagnostics**
  (`textDocument/diagnostic`, gated on a quiescent `experimental/serverStatus` for
  rust-analyzer) — request/response, so a slow server can never read as "clean"; the
  publish+silence-settle path remains only for servers without pull (tsls).
- **rename** via LSP (all references); **replace_node / insert / replace_text** via ranges;
  with `Granularity::Ast`, sub-symbol edits (`#sym:body`, `#sym:return`, `#sym:param.N`)
  work through the same machinery.
- Rejections are **anchored**: each error names the op that introduced it (scoped repair).
- **reindex-on-commit**: `update_index` re-embeds only the changed files.

## Crate status

| Crate | Role | Status |
|---|---|---|
| `ci-core` | types, `LanguageProvider`, `Node` tree + `Granularity`, config, weighting | ✅ |
| `ci-walk` | discovery (gitignore), workspace/package detection | ✅ |
| `ci-embed` | native Model2Vec (tokenizers + safetensors), parity 1.0 vs Python | ✅ |
| `ci-index` | BM25 + vector store + import graph + persistence | ✅ |
| `ci-retrieve` | RRF + graph expansion + package weighting → Manifest | ✅ |
| `ci-scip` | SCIP `index.scip` → Node tree + import graph | ✅ |
| `ci-build` | build pipeline + incremental `update_index` | ✅ |
| `ci-vfs` | in-memory overlay transaction | ✅ |
| `ci-lsp` | **generic** LSP transport (provider supplies the command) | ✅ |
| `ci-edit` | gated atomic edits + anchored repair | ✅ |
| `lang-ts` | TS provider: SCIP + tree-sitter read, LSP+VFS write | ✅ |
| `ci-cli` | `index` / `retrieve` binaries | ✅ |
| `ci-arch` | zero-API architecture map (detects module templates) | ✅ |
| `ci-mcp` | Rust MCP server (stdio): retrieve_context / describe_architecture / list_anchors / apply_edits | ✅ |

**~60 unit tests + real-tool e2e** (scip-typescript indexing, LSP gate, edit gate, the
SCIP+tree-sitter deepen).

## Done vs pending

- **P0** scaffold + core types/trait ✅
- **P1** SCIP-read index + retrieval — real-repo parity vs Node (13/18 on the RRF task) ✅
- **P2** SCIP+LSP+VFS gated edits + anchored repair + incremental reindex ✅
- **SCIP + tree-sitter merge** → `Granularity::Ast`, sub-symbol edits ✅
- **P3:** file ops (`move` via LSP `willRenameFiles`, `delete` via reverse-graph safety) ✅ ·
  `ci-arch` ✅ · `ci-mcp` (Rust MCP server, all 4 tools verified over stdio) ✅ ·
  benchmarks ✅ (`docs/benchmarks.md`)
- **Headline benchmark:** the SCIP+tree-sitter merge costs **+1.2% (~40ms)** at index time;
  Rust indexes ~4× faster than Node; retrieval ≈55% Jaccard overlap (different embedder + graph).

All planned phases (P0–P3 + the SCIP+tree-sitter merge) are complete.

## Verification notes
- Build needs a C compiler (for `tokenizers` + `tree-sitter` grammars) — present on macOS.
- `npx`-based tools run with a fresh `CI_NPM_CACHE` dir to dodge a corrupted default
  `~/.npm` cache (permission/rename errors).
- Model files resolve from `$CI_MODEL_DIR` (Model2Vec dir), default the sibling Node repo's
  `potion-code-16M`.
