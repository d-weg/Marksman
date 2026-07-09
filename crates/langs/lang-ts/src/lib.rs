//! lang-ts — the TypeScript [`LanguageProvider`]. Read path: run `scip-typescript` (via
//! `npx`, no global install) to produce `index.scip`, then serve `structure()` +
//! `import_graph()` from [`ScipIndex`], deepened with tree-sitter sub-nodes. Write path:
//! the tsgo/ts-morph/tsls engine behind the shared `ci_edit::commit_edits` gate.
use ci_core::{
    rel_path, CommitResult, EditOp, EditOpts, Error, FileSummary, Granularity, ImportGraph,
    LanguageProvider, Node, ReadIndex, Result,
};
use ci_edit::Composed;
use ci_scip::ScipIndex;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

mod ablation;
mod ast;
mod engine;
mod fingerprint;
mod outline;
mod tsmorph;

use engine::start_engine;
use fingerprint::{augment_fingerprint, fingerprint_path, fingerprint_drift, load_fingerprint, source_fingerprint, store_fingerprint, Fingerprint};

pub use ablation::TsTreeGated;
pub use outline::outline;

pub(crate) use engine::{npm_cache, NpxCacheLock, SCIP_TS_VERSION};

/// What ONE bare `move_file` covers for TypeScript — composed into the MCP `apply_edits`
/// description by ci-mcp, so the completeness claim the agent reads lives NEXT TO the code
/// that makes it true (the engine's rename/move rewrites) instead of drifting in prose.
pub const MOVE_COVERAGE: &str = "every import specifier (incl. type-only imports) rewritten repo-wide";

/// Everything the TypeScript provider needs from the machine, checked WITHOUT running any of
/// it — the registry builders call this before constructing the provider, so a missing Node
/// yields one actionable message instead of a cryptic npx spawn error mid-index. (A repo with
/// no TypeScript never gets here at all.)
pub fn toolchain() -> ci_core::ToolchainReport {
    let hint = "Node 18+ (https://nodejs.org — e.g. `brew install node`); scip-typescript and ts-morph are then fetched automatically on first use";
    ci_core::ToolchainReport {
        lang: "typescript",
        tools: vec![
            ci_core::ToolStatus {
                tool: "node",
                needed_for: "the type-check gate (ts-morph sidecar) and the language server fallback",
                install: hint,
                found: ci_core::probe_tool(Command::new("node").arg("--version")),
            },
            ci_core::ToolStatus {
                tool: "npx",
                needed_for: "indexing (scip-typescript) and fetching the pinned TS tooling",
                install: hint,
                found: ci_core::probe_tool(Command::new("npx").arg("--version")),
            },
        ],
    }
}

/// The TS/TSX sources the LSP sweep indexes: gitignore-aware walk, `.d.ts` and the usual
/// build/dependency dirs excluded (scip-typescript's own discovery is tsconfig-driven; this
/// walk is the sweep arm's approximation of it).
fn discover_ts_files(root: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).hidden(true).build().flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let rel = match p.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let is_ts = rel.ends_with(".ts") || rel.ends_with(".tsx") || rel.ends_with(".mts") || rel.ends_with(".cts");
        if !is_ts || rel.ends_with(".d.ts") || rel.starts_with("node_modules/") || rel.contains("/node_modules/") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(p) {
            out.push((rel, content));
        }
    }
    out.sort();
    Ok(out)
}

/// The TypeScript provider, assembled from its two halves — [`TsRead`] (the scip-artifact
/// read index the agent plans against) × the write engine (tsgo/ts-morph/tsls, selected in
/// `engine.rs`) — glued by [`Composed`]: post-commit read freshness and the blast-radius
/// policy are derived from the reader's advertised properties (`live`/`semantic_edges`),
/// never hand-wired here. What stays TS-specific in this crate: the scip indexing
/// orchestration + fingerprint cache, the tree-sitter deepen/re-anchor merge, the engine
/// selection ladder, and the npm/NPX cache discipline.
#[derive(Clone)]
pub struct TsProvider {
    // Arc so the provider is cheap to clone out of the MCP server's lock; the SCIP index and
    // the glue (with its warm engine + fresh overrides) are shared, not copied.
    /// Shared with the read half; also held here for the fingerprint augment (`index_with`)
    /// and the prewarmer's warm-file pick.
    scip: Arc<ScipIndex>,
    inner: Arc<Composed<TsRead>>,
}

/// The TypeScript READ half ([`ReadIndex`]): a loaded SCIP artifact (scip-typescript, or
/// the tsgo LSP sweep — same consumer) whose symbols are re-anchored and deepened with
/// tree-sitter sub-nodes against CURRENT disk on every read. An artifact through and
/// through: `live` is false — the loaded index is a startup snapshot, so without the glue's
/// freshness overlay a symbol ADDED by an edit stays invisible and the import graph keeps
/// pre-edit edges until the next reindex — and its edges are `semantic` (the
/// compiler-resolved graph flattens barrels, so the one-hop blast radius is sound —
/// bench T9).
#[derive(Clone)]
struct TsRead {
    root: PathBuf,
    scip: Arc<ScipIndex>,
}

impl ReadIndex for TsRead {
    fn granularity(&self) -> Granularity {
        Granularity::Ast // SCIP symbols + tree-sitter sub-nodes
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        let rel = rel_path(&self.root, file);
        Ok(deepen_from_disk(&self.root, &rel, self.scip.structure(&rel)?))
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        self.scip.import_graph()
    }

    fn live(&self) -> bool {
        false
    }

    fn semantic_edges(&self) -> bool {
        true
    }
}

/// The read-time tree-sitter merge: subdivide each SCIP symbol into sub-nodes
/// (params/return/body/doc) and re-anchor symbols whose file drifted since the index was
/// built (`ast::deepen`), from CURRENT disk content. `CI_NO_TREESITTER` skips the merge
/// (SCIP-only — for the benchmark); no content on disk serves the symbols shallow. Both the
/// read half and the glue's fresh-summary path (the [`ci_edit::FreshDeepener`]) run THIS
/// step, so a post-commit override reads at the same AST depth as an artifact read.
fn deepen_from_disk(root: &Path, rel: &str, nodes: Vec<Node>) -> Vec<Node> {
    if std::env::var("CI_NO_TREESITTER").is_ok() {
        return nodes;
    }
    match std::fs::read_to_string(root.join(rel)) {
        Ok(content) => ast::deepen(&content, nodes),
        Err(_) => nodes,
    }
}

/// Re-describe one committed file for the glue's freshness channel — the fallback when the
/// write engine can't re-describe its live project (`file_summaries` -> `None`: the LSP
/// engines, tsgo/tsls; the ts-morph sidecar serves real summaries and never gets here).
/// Tree-sitter on CURRENT disk — the same read shape [`TsTreeGated`] serves — so reads
/// track the commit instead of serving pre-edit state; scip fidelity (flattened barrels,
/// semantic edges) returns at the next reindex. Per-file: only the changed files are
/// parsed, never a whole-repo walk per commit. EVERY changed file gets a summary (a non-TS
/// file simply reads back empty), so no committed change can be served stale.
fn file_summary(root: &Path, rel: &str) -> FileSummary {
    let fb = lang_fallback::FallbackProvider::new(root, lang_fallback::FbLang::Ts);
    let deleted = !root.join(rel).exists();
    let (nodes, imports) = if deleted {
        (vec![], vec![])
    } else {
        (LanguageProvider::structure(&fb, Path::new(rel)).unwrap_or_default(), fb.file_imports(rel))
    };
    FileSummary { path: rel.into(), deleted, nodes, imports }
}

/// Builds the write engine (lazily in `apply_edits`, or via the prewarmer): the selection
/// ladder in `engine.rs` (tsgo → ts-morph → tsls), with its toolchain-aware error.
fn engine_factory() -> ci_edit::EngineFactory {
    Arc::new(|root: &Path| start_engine(root))
}

impl TsProvider {
    /// Open a provider for `root`, loading the cached `.marksman/index.scip` (milliseconds)
    /// when the source is byte-identical to what produced it, else reindexing (~20s). The
    /// freshness check is the full source fingerprint (see `fingerprint.rs`), so content
    /// edits, import changes, and added/removed/moved files all invalidate; anything doubtful
    /// (no fingerprint, unreadable index) reindexes — a stale load is a correctness bug, a
    /// spurious reindex only a slow start.
    pub fn open(root: &Path) -> Result<Self> {
        let out = root.join(".marksman").join("index.scip");
        let current = source_fingerprint(root);
        if out.exists() {
            match load_fingerprint(&fingerprint_path(root)) {
                Some(stored) => match fingerprint_drift(root, &stored, &current) {
                    None => match Self::from_index(root, &out) {
                        Ok(p) => {
                            eprintln!("[lang-ts] loaded cached {} (source unchanged)", out.display());
                            return Ok(p);
                        }
                        Err(e) => eprintln!("[lang-ts] cached index.scip unreadable ({e}); reindexing"),
                    },
                    Some(why) => eprintln!("[lang-ts] source changed since index.scip was built ({why}); reindexing"),
                },
                None => eprintln!("[lang-ts] no fingerprint for existing index.scip; reindexing"),
            }
        }
        Self::index_with(root, current)
    }

    /// Index `root` with scip-typescript (`npx @sourcegraph/scip-typescript`), then load it.
    /// Always reindexes; `open` is the cached path.
    pub fn index(root: &Path) -> Result<Self> {
        Self::index_with(root, source_fingerprint(root))
    }

    /// The fingerprint is computed by the caller BEFORE scip runs: if a file changes while the
    /// indexer is running, the stored fingerprint reflects the pre-change bytes, so the next
    /// `open` sees a mismatch and reindexes (conservative), rather than blessing an index that
    /// missed the mid-run edit.
    fn index_with(root: &Path, fp: Fingerprint) -> Result<Self> {
        let out = root.join(".marksman").join("index.scip");
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Serialize the npx invocation against other MCP instances sharing this npm cache, so a
        // concurrent `npx` staging can't corrupt the scip-typescript install out from under us.
        let _cache_lock = NpxCacheLock::acquire();
        let status = Command::new("npx")
            .arg("--yes")
            .arg(format!("@sourcegraph/scip-typescript@{SCIP_TS_VERSION}"))
            .args(["index", "--infer-tsconfig", "--no-progress-bar", "--output"])
            .arg(&out)
            .current_dir(root)
            .env("npm_config_cache", npm_cache())
            // Discard the indexer's stdout — it must never pollute an MCP/JSON-RPC stream.
            .stdout(Stdio::null())
            .status()
            .map_err(|e| Error::Driver(format!("launching scip-typescript via npx failed: {e}")))?;
        drop(_cache_lock);
        if !status.success() {
            return Err(Error::Driver(format!("scip-typescript index failed ({status})")));
        }
        let provider = Self::from_index(root, &out)?;
        // Augment with files scip indexed that the walk can't see (gitignored/hidden sources a
        // tsconfig still includes) so their edits invalidate the cache too. Hashed AFTER the
        // run, so the conservative pre-run guarantee narrows to just these hidden files.
        let mut fp = fp;
        augment_fingerprint(&mut fp, root, provider.scip.documents());
        if let Err(e) = store_fingerprint(&fingerprint_path(root), &fp) {
            // Not fatal: without a fingerprint the next `open` just reindexes.
            eprintln!("[lang-ts] could not persist the index fingerprint ({e}); next startup will reindex");
        }
        Ok(provider)
    }

    /// Index `root` by SWEEPING the tsgo language server (documentSymbol + references via
    /// [`ci_lsp_index`]) instead of running scip-typescript — the `CI_TS_MODE=lsp` comparison
    /// arm. Emits a genuine SCIP protobuf to `.marksman/index.lspx.scip`, so the whole read
    /// path (structure, import graph, blast radius) is byte-for-byte the same consumer as the
    /// scip-typescript index. No fingerprint cache yet: this arm always re-sweeps.
    pub fn index_with_lsp_sweep(root: &Path) -> Result<Self> {
        let files = discover_ts_files(root)?;
        let bytes = ci_lsp_index::sweep_index(root, &files, engine::tsgo_lsp_command(), "lspx-ts")?;
        let out = root.join(".marksman").join("index.lspx.scip");
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&out, bytes)?;
        Self::from_index(root, &out)
    }

    /// Load a provider from an existing `index.scip` (skip running the indexer).
    pub fn from_index(root: &Path, index_scip: &Path) -> Result<Self> {
        Ok(Self::assemble(root, Arc::new(ScipIndex::load(index_scip)?), engine_factory()))
    }

    /// Wire the halves: the read index advertises its properties (artifact, semantic
    /// edges), the engine factory carries the selection ladder, the prewarmer issues the
    /// project-loading warm call, the live summarizer re-describes committed files from
    /// tree-sitter, and the fresh deepener keeps post-commit overrides at AST depth. The
    /// factory is a parameter so the freshness seams are testable without Node.
    fn assemble(root: &Path, scip: Arc<ScipIndex>, engine_factory: ci_edit::EngineFactory) -> Self {
        let read = TsRead { root: root.to_path_buf(), scip: scip.clone() };
        let sum_root = root.to_path_buf();
        let deep_root = root.to_path_buf();
        let warm_scip = scip.clone();
        let warm_factory = engine_factory.clone();
        let inner = Composed::new(root, read, engine_factory)
            .with_live_summarizer(Arc::new(move |rel| Some(file_summary(&sum_root, rel))))
            .with_fresh_deepener(Arc::new(move |rel, nodes| deepen_from_disk(&deep_root, rel, nodes)))
            .with_prewarmer(Arc::new(move |root: &Path| {
                let mut engine = warm_factory(root).ok()?;
                // A real source file to open: LSP needs it to load the tsconfig project (an
                // empty diagnostics short-circuits); ts-morph loads at startup, so the
                // round-trip just confirms it's ready.
                let warm_file = warm_scip
                    .import_graph()
                    .ok()
                    .and_then(|g| g.into_keys().next())
                    .map(|p| p.to_string_lossy().replace('\\', "/"));
                match warm_file.and_then(|f| std::fs::read_to_string(root.join(&f)).ok().map(|c| (f, c))) {
                    Some(file) => {
                        let _ = engine.diagnostics(&[file]); // forces the project to load
                    }
                    None => {
                        let _ = engine.diagnostics(&[]);
                    }
                }
                Some(engine)
            }));
        Self { scip, inner: Arc::new(inner) }
    }
}

impl LanguageProvider for TsProvider {
    fn granularity(&self) -> Granularity {
        self.inner.granularity()
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        self.inner.structure(file)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        self.inner.import_graph()
    }

    /// Start the write engine and load the project NOW, on a background thread, so the first
    /// `apply_edits` finds it warm instead of paying the ~seconds cold project load inline.
    /// The recipe is the [`ci_edit::Prewarmer`] wired in [`TsProvider::assemble`]; the
    /// lock/wait/no-double-start discipline lives in `ci_edit::spawn_prewarm`, driven by the
    /// glue. Safe no-op if the engine can't start (apply_edits starts one fresh).
    fn prewarm(&self) {
        self.inner.prewarm()
    }

    /// The write path is [`Composed`]: persistent warm engine (reuse from prewarm), the
    /// shared VFS + baseline-diff + blast-radius spine, anchors resolved from the FULL
    /// structure the agent saw (SCIP + tree-sitter sub-nodes, fresh overrides included),
    /// radius policy from the read half's semantic edges (one hop), and post-commit read
    /// freshness from `file_summaries` with the tree-sitter summarizer as the LSP-engine
    /// fallback.
    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        self.inner.apply_edits(ops, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_core::Diag;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Mutex;

    /// A scripted gate: always-clean diagnostics; each `file_summaries` call pops the next
    /// scripted payload (`None` = "engine can't re-describe", the LSP-engine shape) — the
    /// freshness channel under test, no Node toolchain required.
    struct ScriptedEngine(Arc<Mutex<VecDeque<Option<Vec<FileSummary>>>>>);

    impl ci_edit::GateEngine for ScriptedEngine {
        fn diagnostics(&mut self, _files: &[(String, String)]) -> Result<Vec<Diag>> {
            Ok(vec![])
        }
        fn rename(&mut self, _f: &str, _l: u32, _c: u32, _n: &str) -> Result<Value> {
            Ok(json!({}))
        }
        fn will_rename(&mut self, _from: &str, _to: &str) -> Result<Value> {
            Ok(json!({}))
        }
        fn file_summaries(&mut self, _files: &[String]) -> Result<Option<Vec<FileSummary>>> {
            Ok(self.0.lock().unwrap().pop_front().unwrap_or(None))
        }
    }

    /// Commit one trivial in-file replace so the freshness channel fires.
    fn commit_replace(p: &TsProvider, old_text: &str, new_text: &str) {
        let r = p
            .apply_edits(
                &[EditOp::ReplaceInFile { path: "a.ts".into(), old_text: old_text.into(), new_text: new_text.into() }],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        assert!(matches!(r, CommitResult::Ok { .. }), "clean commit expected: {r:?}");
    }

    // The post-edit read override (contract §2, reads stay true in-session), driven through
    // the Composed glue: an engine summary must win over the (empty) SCIP index for
    // structure() — still deepened from current disk content (the fresh deepener) — and
    // rewrite that file's outgoing import edges; a deleted summary must blank both; and when
    // the engine CAN'T re-describe (`file_summaries` -> None, the LSP engines), the
    // tree-sitter live summarizer must re-describe current disk instead, so reads never
    // serve pre-edit state.
    #[test]
    fn fresh_overrides_shadow_the_scip_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".marksman")).unwrap();
        let idx = root.join(".marksman/index.scip");
        fs::write(&idx, b"").unwrap(); // valid, empty SCIP index
        fs::write(root.join("a.ts"), "export function add(a: number): number {\n  return a;\n}\n").unwrap();

        let node = Node {
            id: "a.ts#add".into(),
            name: Some("add".into()),
            kind: ci_core::NodeKind::Symbol(ci_core::SymbolKind::Function),
            range: ci_core::Range { start_line: 1, start_char: 0, end_line: 3, end_char: 1 },
            name_range: Some(ci_core::Range { start_line: 1, start_char: 16, end_line: 1, end_char: 19 }),
            children: vec![],
        };
        let script: Arc<Mutex<VecDeque<Option<Vec<FileSummary>>>>> =
            Arc::new(Mutex::new(VecDeque::from([
                // commit 1: the engine re-describes a.ts (a shallow symbol + a new edge).
                Some(vec![FileSummary {
                    path: "a.ts".into(),
                    deleted: false,
                    nodes: vec![node],
                    imports: vec![PathBuf::from("b.ts")],
                }]),
                // commit 2: the engine says the file is gone.
                Some(vec![FileSummary { path: "a.ts".into(), deleted: true, nodes: vec![], imports: vec![] }]),
                // commit 3: the engine can't re-describe -> the live summarizer must.
                None,
            ])));
        let feed = script.clone();
        let p = TsProvider::assemble(
            root,
            Arc::new(ScipIndex::load(&idx).unwrap()),
            Arc::new(move |_root: &Path| {
                Ok(Box::new(ScriptedEngine(feed.clone())) as Box<dyn ci_edit::GateEngine + Send>)
            }),
        );
        assert!(p.structure(Path::new("a.ts")).unwrap().is_empty(), "empty index, no override yet");

        commit_replace(&p, "return a;", "return a; // one");
        let nodes = p.structure(Path::new("a.ts")).unwrap();
        let add = nodes.iter().find(|n| n.id == "a.ts#add").expect("override symbol served");
        assert!(add.children.iter().any(|c| c.id == "a.ts#add:body"), "override still deepened: {:?}", add.children);
        let g = p.import_graph().unwrap();
        assert_eq!(g.get(&PathBuf::from("a.ts")).unwrap(), &vec![PathBuf::from("b.ts")], "override edge served");

        commit_replace(&p, "// one", "// two");
        assert!(p.structure(Path::new("a.ts")).unwrap().is_empty(), "deleted override blanks structure");
        assert!(!p.import_graph().unwrap().contains_key(&PathBuf::from("a.ts")), "deleted override removes edges");

        commit_replace(&p, "// two", "// three");
        let nodes = p.structure(Path::new("a.ts")).unwrap();
        let add = nodes.iter().find(|n| n.id == "a.ts#add").expect("live summarizer re-described the file");
        assert!(
            add.children.iter().any(|c| c.id == "a.ts#add:body"),
            "summarizer-served structure carries sub-node anchors: {:?}",
            add.children
        );
    }

    // Real end-to-end: shells out to scip-typescript via npx. Slow + network on
    // first run, so #[ignore] — run explicitly with `cargo test -p lang-ts -- --ignored`.
    #[test]
    #[ignore]
    fn indexes_real_ts_project_via_scip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"Bundler","strict":true},"include":["src"]}"#,
        )
        .unwrap();
        fs::write(
            root.join("src/math.ts"),
            "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/app.ts"),
            "import { add } from \"./math.js\";\nexport function main(): number {\n  return add(1, 2);\n}\n",
        )
        .unwrap();

        let provider = TsProvider::index(root).expect("scip-typescript indexing");

        // structure(math.ts) surfaces the `add` function with a real enclosing range.
        let nodes = provider.structure(Path::new("src/math.ts")).unwrap();
        let add = nodes.iter().find(|n| n.name.as_deref() == Some("add")).expect("add symbol");
        assert!(matches!(add.kind, ci_core::NodeKind::Symbol(ci_core::SymbolKind::Function)));
        assert!(add.range.end_line >= add.range.start_line && add.range.start_line >= 1);

        // import graph: app.ts references add (in math.ts) -> edge app -> math.
        let g = provider.import_graph().unwrap();
        let app = g.get(&PathBuf::from("src/app.ts")).expect("app.ts edges");
        assert!(app.contains(&PathBuf::from("src/math.ts")));

        // Unchanged source -> `open` loads the cached index.scip instead of re-running scip
        // (the file's mtime must not move — reindexing rewrites it).
        let scip = root.join(".marksman/index.scip");
        let cached_mtime = fs::metadata(&scip).unwrap().modified().unwrap();
        let reopened = TsProvider::open(root).expect("open from cache");
        assert_eq!(fs::metadata(&scip).unwrap().modified().unwrap(), cached_mtime, "open() re-ran the indexer on unchanged source");
        assert!(reopened.structure(Path::new("src/math.ts")).unwrap().iter().any(|n| n.name.as_deref() == Some("add")));

        // A source edit invalidates the fingerprint -> `open` reindexes and sees the new symbol.
        fs::write(
            root.join("src/math.ts"),
            "export function add(a: number, b: number): number {\n  return a + b;\n}\nexport function sub(a: number, b: number): number {\n  return a - b;\n}\n",
        )
        .unwrap();
        let refreshed = TsProvider::open(root).expect("open after edit reindexes");
        assert!(refreshed.structure(Path::new("src/math.ts")).unwrap().iter().any(|n| n.name.as_deref() == Some("sub")));
    }

    // The treesitter-gated ablation provider: no scip anywhere, tree-sitter reads, and the
    // SAME warm ts-morph gate — a type-breaking edit must reject, a cross-file rename must
    // still land everywhere (the compiler finds references even though reads are syntactic).
    // #[ignore]; `cargo test -p lang-ts -- --ignored`.
    #[test]
    #[ignore]
    fn treesitter_gated_gates_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"Bundler","strict":true},"include":["src"]}"#,
        )
        .unwrap();
        fs::write(root.join("src/math.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n").unwrap();
        fs::write(root.join("src/app.ts"), "import { add } from \"./math\";\nexport const r = add(1, 2);\n").unwrap();

        let p = TsTreeGated::new(root);
        // tree-sitter read path sees the symbols with no scip index anywhere.
        assert!(!root.join(".marksman/index.scip").exists());
        assert!(p.structure(Path::new("src/math.ts")).unwrap().iter().any(|n| n.id == "src/math.ts#add"));
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        // Type-breaking edit -> REJECTED (the gate is real).
        let bad = p
            .apply_edits(&[EditOp::SetBody { node_id: "src/math.ts#add".into(), body: "{\n  \"nope\"\n}".into() }], &opts)
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "type error must reject: {bad:?}");

        // Cross-file rename -> both files rewritten (ts-morph finds references).
        let ok = p
            .apply_edits(&[EditOp::Rename { node_id: "src/math.ts#add".into(), new_name: "sum".into() }], &opts)
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "rename commits: {ok:?}");
        assert!(fs::read_to_string(root.join("src/app.ts")).unwrap().contains("sum(1, 2)"), "caller renamed");
        assert!(!fs::read_to_string(root.join("src/math.ts")).unwrap().contains("add"), "definition renamed");
    }

    // The barrel blast-radius claim, verified end-to-end (this is what bench task T9 measures):
    // the gate expands ONE reverse-import hop, so it only reaches a consumer importing through a
    // barrel (`export *`) if the graph flattens the barrel. SCIP's semantic graph does — the
    // consumer edges DIRECTLY to the defining file, and adding a required interface field rejects
    // naming the consumer's construction site. The syntactic graph does not: the consumer edges to
    // the barrel, the barrel itself never errors on a new required field, and the same edit
    // COMMITS "clean" while the consumer no longer compiles — the accepted residual of
    // treesitter-gated mode. #[ignore]; `cargo test -p lang-ts -- --ignored`.
    #[test]
    #[ignore]
    fn barrel_consumer_inside_scip_blast_radius_outside_syntactic() {
        let write_fixture = |root: &Path| {
            fs::create_dir_all(root.join("src/core")).unwrap();
            fs::write(
                root.join("tsconfig.json"),
                r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"Bundler","strict":true},"include":["src"]}"#,
            )
            .unwrap();
            fs::write(
                root.join("src/core/policy.ts"),
                "export interface QuotaPolicy {\n  name: string;\n  limit: number;\n}\n\nexport function defaultPolicy(name: string): QuotaPolicy {\n  return { name, limit: 100 };\n}\n",
            )
            .unwrap();
            fs::write(root.join("src/core/index.ts"), "export * from \"./policy\";\n").unwrap();
            fs::write(
                root.join("src/app.ts"),
                "import { QuotaPolicy } from \"./core\";\nexport const anon: QuotaPolicy = { name: \"anon\", limit: 20 };\n",
            )
            .unwrap();
        };
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let burst = EditOp::InsertMember { node_id: "src/core/policy.ts#QuotaPolicy".into(), code: "burst: number;".into() };

        // FULL (scip): the barrel is flattened — app.ts edges directly to policy.ts — so the
        // one-hop gate reaches the consumer and the reject names its construction site.
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let full = TsProvider::index(dir.path()).expect("scip-typescript indexing");
        let g = full.import_graph().unwrap();
        let app_edges = g.get(&PathBuf::from("src/app.ts")).expect("app.ts edges");
        assert!(
            app_edges.contains(&PathBuf::from("src/core/policy.ts")),
            "scip graph must flatten the barrel: {app_edges:?}"
        );
        match full.apply_edits(std::slice::from_ref(&burst), &opts).unwrap() {
            CommitResult::Rejected { feedback, .. } => {
                assert!(feedback.contains("src/app.ts"), "reject must name the barrel consumer:\n{feedback}")
            }
            other => panic!("required field through a barrel must reject in full mode: {other:?}"),
        }

        // TREESITTER-GATED (syntactic graph): app.ts edges to the barrel, not policy.ts — the
        // graph itself can't flatten `export *`. The gate compensates by serving the TRANSITIVE
        // reverse-importer set (the T9-barrel fix), so the same edit must reject naming app.ts
        // just like scip mode — a barrel must never hide a consumer from the gate.
        let dir2 = tempfile::tempdir().unwrap();
        write_fixture(dir2.path());
        let gated = TsTreeGated::new(dir2.path());
        let g = gated.import_graph().unwrap();
        let app_edges = g.get(&PathBuf::from("src/app.ts")).expect("app.ts edges");
        assert!(
            !app_edges.contains(&PathBuf::from("src/core/policy.ts")),
            "syntactic graph must NOT flatten the barrel (else this test guards nothing): {app_edges:?}"
        );
        match gated.apply_edits(std::slice::from_ref(&burst), &opts).unwrap() {
            CommitResult::Rejected { feedback, .. } => {
                assert!(feedback.contains("src/app.ts"), "the transitive radius must reach through the barrel:\n{feedback}")
            }
            other => panic!("a barrel-hidden consumer must still reject in gated mode: {other:?}"),
        }
        // A batch that fixes only the same-file literal must STILL reject (app.ts is broken) …
        let partial = [
            burst.clone(),
            EditOp::ReplaceNode {
                node_id: "src/core/policy.ts#defaultPolicy".into(),
                // The fallback node range starts at `function` — the `export` keyword stays.
                code: "function defaultPolicy(name: string): QuotaPolicy {\n  return { name, limit: 100, burst: 0 };\n}".into(),
            },
        ];
        match gated.apply_edits(&partial, &opts).unwrap() {
            CommitResult::Rejected { feedback, .. } => {
                assert!(feedback.contains("src/app.ts"), "the untouched consumer still blocks:\n{feedback}")
            }
            other => panic!("a partial fix must not commit: {other:?}"),
        }
        // … and the complete batch (consumer included) commits everywhere.
        let complete = [
            partial[0].clone(),
            partial[1].clone(),
            EditOp::ReplaceText {
                node_id: "src/app.ts#anon".into(),
                old_text: "limit: 20".into(),
                new_text: "limit: 20, burst: 0".into(),
            },
        ];
        match gated.apply_edits(&complete, &opts).unwrap() {
            CommitResult::Ok { .. } => {}
            other => panic!("the complete fix must commit: {other:?}"),
        }
        let app = fs::read_to_string(dir2.path().join("src/app.ts")).unwrap();
        assert!(app.contains("burst: 0"), "consumer updated in the same gated batch:\n{app}");
    }

    // The monorepo seam, verified end-to-end (bench task T10 measures the agent-visible cost):
    // a workspace consumer imports through a BARE specifier ("@acme/core", resolved by root
    // tsconfig `paths`). The syntactic resolver follows only RELATIVE specifiers — a bare one is
    // indistinguishable from a third-party package — so the fallback graph has NO cross-package
    // edge and no transitive closure can recover it: in gated mode the consumer sits outside the
    // gate and a breaking commit claims clean. SCIP resolves the alias via the TS compiler, so
    // in full mode the consumer is one semantic hop away and the reject names it. This residual
    // is STRUCTURAL for the syntactic tier (an edge-existence problem, not a radius-depth one);
    // the fix is scip — that's the point. #[ignore]; `cargo test -p lang-ts -- --ignored`.
    #[test]
    #[ignore]
    fn monorepo_bare_specifier_consumer_inside_scip_radius_invisible_to_syntactic() {
        let write_fixture = |root: &Path| {
            fs::create_dir_all(root.join("packages/core/src")).unwrap();
            fs::create_dir_all(root.join("packages/gateway/src")).unwrap();
            fs::write(root.join("package.json"), r#"{"name":"acme","private":true,"workspaces":["packages/*"]}"#).unwrap();
            fs::write(
                root.join("tsconfig.json"),
                r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"Bundler","strict":true,"noEmit":true,"baseUrl":".","paths":{"@acme/core":["packages/core/src/index.ts"]}},"include":["packages"]}"#,
            )
            .unwrap();
            fs::write(
                root.join("packages/core/src/policy.ts"),
                "export interface RetryPolicy {\n  maxAttempts: number;\n}\n\nexport function defaultRetry(): RetryPolicy {\n  return { maxAttempts: 3 };\n}\n",
            )
            .unwrap();
            fs::write(root.join("packages/core/src/index.ts"), "export * from \"./policy\";\n").unwrap();
            fs::write(
                root.join("packages/gateway/src/proxy.ts"),
                "import { RetryPolicy } from \"@acme/core\";\nexport const aggressive: RetryPolicy = { maxAttempts: 6 };\n",
            )
            .unwrap();
        };
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let field = EditOp::InsertMember {
            node_id: "packages/core/src/policy.ts#RetryPolicy".into(),
            code: "timeoutMs: number;".into(),
        };

        // FULL (scip): the bare specifier resolves through the tsconfig alias — the consumer
        // edges across the package boundary, and the reject names its construction site.
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let full = TsProvider::index(dir.path()).expect("scip-typescript indexing");
        let g = full.import_graph().unwrap();
        let proxy_edges = g.get(&PathBuf::from("packages/gateway/src/proxy.ts")).expect("proxy.ts edges");
        assert!(
            proxy_edges.contains(&PathBuf::from("packages/core/src/policy.ts")),
            "scip must resolve the bare workspace specifier cross-package: {proxy_edges:?}"
        );
        match full.apply_edits(std::slice::from_ref(&field), &opts).unwrap() {
            CommitResult::Rejected { feedback, .. } => assert!(
                feedback.contains("packages/gateway/src/proxy.ts"),
                "reject must name the cross-package consumer:\n{feedback}"
            ),
            other => panic!("required field consumed cross-package must reject in full mode: {other:?}"),
        }

        // TREESITTER-GATED: no edge exists for a bare specifier, so the consumer is invisible —
        // the core-internal reject fires, but fixing only what it shows commits "clean" while
        // the gateway no longer compiles. Structural residual; scip is the fix.
        let dir2 = tempfile::tempdir().unwrap();
        write_fixture(dir2.path());
        let gated = TsTreeGated::new(dir2.path());
        let g = gated.import_graph().unwrap();
        assert!(
            !g.contains_key(&PathBuf::from("packages/gateway/src/proxy.ts")),
            "syntactic graph must have no bare-specifier edge (else this test guards nothing): {g:?}"
        );
        let batch = [
            field,
            EditOp::ReplaceNode {
                node_id: "packages/core/src/policy.ts#defaultRetry".into(),
                code: "function defaultRetry(): RetryPolicy {\n  return { maxAttempts: 3, timeoutMs: 1000 };\n}".into(),
            },
        ];
        match gated.apply_edits(&batch, &opts).unwrap() {
            CommitResult::Ok { .. } => {}
            other => panic!("the consumer is invisible to the syntactic tier, so this must commit: {other:?}"),
        }
        let proxy = fs::read_to_string(dir2.path().join("packages/gateway/src/proxy.ts")).unwrap();
        assert!(!proxy.contains("timeoutMs"), "consumer untouched — committed 'clean' across a broken package boundary");
    }

    // Real end-to-end for the post-edit read refresh (scip-typescript + node + ts-morph):
    // WITHOUT re-running the indexer, a committed edit must make (a) a NEW symbol visible to
    // structure() — impossible before, reanchor can't invent nodes — and (b) a NEW file's
    // import edge visible to import_graph(). #[ignore]; `cargo test -p lang-ts -- --ignored`.
    #[test]
    #[ignore]
    fn committed_edit_refreshes_reads_in_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"Bundler","strict":true},"include":["src"]}"#,
        )
        .unwrap();
        fs::write(
            root.join("src/math.ts"),
            "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
        )
        .unwrap();

        let provider = TsProvider::index(root).expect("scip-typescript indexing");
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let res = provider
            .apply_edits(
                &[
                    // A NEW symbol in an existing file…
                    EditOp::ReplaceNode {
                        node_id: "src/math.ts#add".into(),
                        code: "export function add(a: number, b: number): number {\n  return a + b;\n}\nexport function sub(a: number, b: number): number {\n  return a - b;\n}".into(),
                    },
                    // …and a NEW file importing it.
                    EditOp::CreateFile {
                        path: "src/calc.ts".into(),
                        code: "import { sub } from \"./math.js\";\nexport const d = sub(3, 1);\n".into(),
                    },
                ],
                &opts,
            )
            .expect("apply_edits");
        assert!(matches!(res, CommitResult::Ok { .. }), "edit must commit: {res:?}");

        // No reindex, same provider instance: the new symbol and the new edge are visible.
        let math = provider.structure(Path::new("src/math.ts")).unwrap();
        let sub = math.iter().find(|n| n.id == "src/math.ts#sub").expect("NEW symbol visible post-edit");
        assert!(sub.children.iter().any(|c| c.id == "src/math.ts#sub:body"), "new symbol deepened");
        let calc = provider.structure(Path::new("src/calc.ts")).unwrap();
        assert!(calc.iter().any(|n| n.id == "src/calc.ts#d"), "new FILE's symbols visible: {calc:?}");
        let g = provider.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("src/calc.ts")).expect("new file has graph edges");
        assert!(edges.contains(&PathBuf::from("src/math.ts")), "new import edge visible: {edges:?}");
    }

    // Rename parity across gate engines: the SAME cross-file rename through ts-morph and
    // through tsgo must leave byte-identical trees — the check that gates flipping the
    // default engine to tsgo. Needs CI_TSGO (skips otherwise); mutates CI_EDIT_ENGINE, so
    // run the ignored tier single-threaded if adding more env-dependent tests.
    // #[ignore]; `CI_TSGO=… cargo test -p lang-ts -- --ignored rename_parity`
    #[test]
    #[ignore]
    fn rename_parity_tsmorph_vs_tsgo() {
        if std::env::var("CI_TSGO").is_err() {
            eprintln!("SKIP rename_parity: set CI_TSGO to a tsgo binary");
            return;
        }
        let write_fixture = |root: &Path| {
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(
                root.join("tsconfig.json"),
                r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"Bundler","strict":true,"noEmit":true},"include":["src"]}"#,
            )
            .unwrap();
            fs::write(
                root.join("src/math.ts"),
                "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
            )
            .unwrap();
            fs::write(
                root.join("src/app.ts"),
                "import { add } from \"./math.js\";\nexport const total = add(1, 2);\nexport const twice = add(total, total);\n",
            )
            .unwrap();
        };
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let rename = EditOp::Rename { node_id: "src/math.ts#add".into(), new_name: "sum".into() };

        // Restores CI_EDIT_ENGINE even if apply_edits panics — a leaked forced engine would
        // silently change what every later ignored test measures.
        struct EnvGuard(Option<String>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("CI_EDIT_ENGINE", v),
                    None => std::env::remove_var("CI_EDIT_ENGINE"),
                }
            }
        }
        let run_with_engine = |engine: &str| -> (String, String) {
            let _guard = EnvGuard(std::env::var("CI_EDIT_ENGINE").ok());
            std::env::set_var("CI_EDIT_ENGINE", engine);
            let dir = tempfile::tempdir().unwrap();
            write_fixture(dir.path());
            let p = TsProvider::index(dir.path()).expect("scip-typescript indexing");
            let res = p.apply_edits(std::slice::from_ref(&rename), &opts).unwrap();
            assert!(matches!(res, CommitResult::Ok { .. }), "[{engine}] rename must commit: {res:?}");
            (
                fs::read_to_string(dir.path().join("src/math.ts")).unwrap(),
                fs::read_to_string(dir.path().join("src/app.ts")).unwrap(),
            )
        };

        let (math_m, app_m) = run_with_engine("tsmorph");
        let (math_g, app_g) = run_with_engine("tsgo");
        assert!(math_m.contains("sum") && !math_m.contains("add"), "definition renamed:\n{math_m}");
        assert!(app_m.contains("sum(1, 2)"), "caller renamed:\n{app_m}");
        assert_eq!(math_m, math_g, "math.ts must be byte-identical across engines");
        assert_eq!(app_m, app_g, "app.ts must be byte-identical across engines");
    }
}
