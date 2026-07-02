//! lang-ts — the TypeScript [`LanguageProvider`]. v1 read path: run
//! `scip-typescript` (via `npx`, no global install) to produce `index.scip`, then
//! serve `structure()` + `import_graph()` from [`ScipIndex`]. The write path
//! (VFS + LSP gate) lands in P2.
use ci_core::{
    CommitResult, EditOp, EditOpts, Error, FileSummary, Granularity, ImportGraph, LanguageProvider,
    Node, Result,
};
use ci_edit::GateEngine;
use ci_scip::ScipIndex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

mod ast;
mod fingerprint;
mod outline;
mod tsmorph;

use fingerprint::{fingerprint_path, fingerprint_drift, hash_file, load_fingerprint, source_fingerprint, store_fingerprint, Fingerprint};

pub use outline::outline;

/// Pinned TS toolchain. Unpinned npx/npm floats to "latest", which drifts under us: a new
/// scip-typescript can change index content between two startups (silently invalidating the
/// cache semantics), and a new tsserver/typescript changes what the gate accepts. Bump these
/// deliberately; `SCIP_TS_VERSION` participates in the source fingerprint, so bumping it
/// reindexes on the next startup.
pub(crate) const SCIP_TS_VERSION: &str = "0.4.0";
const TS_LSP_VERSION: &str = "5.3.0";
const TYPESCRIPT_VERSION: &str = "6.0.3";

/// Fresh npm cache dir so a corrupted default `~/.npm` cache can't break `npx`. Shared with the
/// ts-morph sidecar (`tsmorph.rs`) so both TS tooling paths use the same cache location.
pub(crate) fn npm_cache() -> PathBuf {
    std::env::var("CI_NPM_CACHE").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("ci-npm-cache"))
}

/// A best-effort cross-process advisory lock so concurrent `npx` invocations don't corrupt the
/// SHARED npm cache. `npx --yes` stages packages into `<cache>/_npx/<hash>` with atomic renames;
/// two invocations racing there produce `ENOTEMPTY` / half-installed packages (`Cannot find module
/// './Counter'`), so scip-typescript fails intermittently whenever several MCP instances start at
/// once (an agent benchmark, or a few editor sessions). Held for the npx run, released on drop.
/// Best-effort: a stale lock (crashed holder) is stolen after 5 min, and we give up waiting after
/// 3 min and proceed unlocked rather than ever hang the tool.
pub(crate) struct NpxCacheLock(PathBuf);

impl NpxCacheLock {
    pub(crate) fn acquire() -> Option<Self> {
        let dir = npm_cache();
        let _ = std::fs::create_dir_all(&dir);
        let lock = dir.join(".npx.lock");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => return Some(NpxCacheLock(lock)),
                Err(_) => {
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|e| e.as_secs() > 300);
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }
}

impl Drop for NpxCacheLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A persistent, warmed-once gate engine (ts-morph sidecar or LSP server), reused across
/// edits. The whole reason rust `apply_edits` was 68s was a COLD engine per call (project
/// typecheck from scratch); keeping one warm here is the fix. Behind a Mutex so [`prewarm`]
/// can load the project on a background thread while the agent is still searching/thinking.
type WarmEngine = Arc<Mutex<Option<Box<dyn GateEngine + Send>>>>;

#[derive(Clone)]
pub struct TsProvider {
    root: PathBuf,
    // Arc so the provider is cheap to clone out of the MCP server's lock; the SCIP index and
    // the warm engine are shared, not copied.
    scip: Arc<ScipIndex>,
    engine: WarmEngine,
    /// Per-file read overrides captured from the write engine right after a committed edit
    /// (see `GateEngine::file_summaries`). The loaded SCIP index is a startup artifact: without
    /// this, a symbol ADDED by an edit stays invisible to structure()/list_anchors and the
    /// import graph keeps pre-edit edges until the next reindex. Keyed by repo-relative path;
    /// consulted before the SCIP index, cleared implicitly by the next startup (re)index.
    fresh: Arc<Mutex<HashMap<String, FileSummary>>>,
}

/// Start the lightest available write engine for `root`: ts-morph in-process (synchronous,
/// no LSP settle race) when its sidecar can start, else the generic LSP server. Override with
/// `CI_EDIT_ENGINE=lsp|tsmorph`.
fn start_engine(root: &Path) -> Result<Box<dyn GateEngine + Send>> {
    let pref = std::env::var("CI_EDIT_ENGINE").unwrap_or_default();
    if pref != "lsp" {
        match tsmorph::TsMorphClient::start(root) {
            Ok(c) => return Ok(Box::new(c)),
            Err(e) if pref == "tsmorph" => return Err(e), // forced: surface the failure
            Err(_) => {} // auto: fall back to LSP
        }
    }
    Ok(Box::new(ci_lsp::LspClient::start(root, TsProvider::ts_lsp_command())?))
}

impl TsProvider {
    /// Open a provider for `root`, loading the cached `.codeindex/index.scip` (milliseconds)
    /// when the source is byte-identical to what produced it, else reindexing (~20s). The
    /// freshness check is the full source fingerprint (see `fingerprint.rs`), so content
    /// edits, import changes, and added/removed/moved files all invalidate; anything doubtful
    /// (no fingerprint, unreadable index) reindexes — a stale load is a correctness bug, a
    /// spurious reindex only a slow start.
    pub fn open(root: &Path) -> Result<Self> {
        let out = root.join(".codeindex").join("index.scip");
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
        let out = root.join(".codeindex").join("index.scip");
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
        for doc in provider.scip.documents() {
            if !fp.contains_key(&doc) {
                if let Some(h) = hash_file(&root.join(&doc)) {
                    fp.insert(doc, h);
                }
            }
        }
        if let Err(e) = store_fingerprint(&fingerprint_path(root), &fp) {
            // Not fatal: without a fingerprint the next `open` just reindexes.
            eprintln!("[lang-ts] could not persist the index fingerprint ({e}); next startup will reindex");
        }
        Ok(provider)
    }

    /// Load a provider from an existing `index.scip` (skip running the indexer).
    pub fn from_index(root: &Path, index_scip: &Path) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            scip: Arc::new(ScipIndex::load(index_scip)?),
            engine: Arc::new(Mutex::new(None)),
            fresh: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The TS language-server command (npx tsls). All external/Node tooling lives
    /// here in the provider — the core + ci-lsp stay pure Rust.
    fn ts_lsp_command() -> Command {
        let mut c = Command::new("npx");
        c.arg("--yes")
            .arg("-p")
            .arg(format!("typescript-language-server@{TS_LSP_VERSION}"))
            .arg("-p")
            .arg(format!("typescript@{TYPESCRIPT_VERSION}"))
            .args(["typescript-language-server", "--stdio"])
            .env("npm_config_cache", npm_cache());
        c
    }

    /// Normalize a (possibly absolute) path to the repo-relative posix form SCIP uses.
    fn rel(&self, file: &Path) -> String {
        let p = if file.is_absolute() {
            file.strip_prefix(&self.root).unwrap_or(file)
        } else {
            file
        };
        p.to_string_lossy().replace('\\', "/")
    }
}

impl LanguageProvider for TsProvider {
    fn granularity(&self) -> Granularity {
        Granularity::Ast // SCIP symbols + tree-sitter sub-nodes
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        let rel = self.rel(file);
        // A post-edit override wins over the startup SCIP index: it's the same file as
        // re-described by the compiler that just gated the edit (new symbols included).
        let fresh = self.fresh.lock().ok().and_then(|m| m.get(&rel).map(|s| (s.deleted, s.nodes.clone())));
        let scip_nodes = match fresh {
            Some((true, _)) => return Ok(vec![]),
            Some((false, nodes)) => nodes,
            None => self.scip.structure(&rel)?,
        };
        // CI_NO_TREESITTER: skip the merge (SCIP-only) — for the benchmark.
        if std::env::var("CI_NO_TREESITTER").is_ok() {
            return Ok(scip_nodes);
        }
        // Merge: deepen each SCIP symbol with tree-sitter sub-nodes (params/return/body).
        match std::fs::read_to_string(self.root.join(&rel)) {
            Ok(content) => Ok(ast::deepen(&content, scip_nodes)),
            Err(_) => Ok(scip_nodes), // no content on disk -> shallow
        }
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        let mut g = self.scip.import_graph()?;
        // Overlay post-edit edges: each override replaces that file's OUTGOING edges (incoming
        // edges live in the importers' own entries, refreshed when those files change).
        if let Ok(m) = self.fresh.lock() {
            for (rel, s) in m.iter() {
                let key = PathBuf::from(rel);
                if s.deleted || s.imports.is_empty() {
                    g.remove(&key);
                } else {
                    g.insert(key, s.imports.clone());
                }
            }
        }
        Ok(g)
    }

    /// Start the write engine and load the project NOW, on a background thread, so the first
    /// `apply_edits` finds it warm instead of paying the ~seconds cold project load inline.
    /// The thread holds the engine lock for the duration, so an `apply_edits` that arrives
    /// mid-warm simply waits for it rather than racing in a second cold engine. Safe no-op if
    /// the engine can't start (apply_edits falls back to starting one fresh).
    fn prewarm(&self) {
        let slot = self.engine.clone();
        let root = self.root.clone();
        // A real source file to open: LSP needs it to load the tsconfig project (an empty
        // diagnostics short-circuits); ts-morph loads at startup, so the round-trip just
        // confirms it's ready.
        let warm_file = self
            .scip
            .import_graph()
            .ok()
            .and_then(|g| g.into_keys().next())
            .map(|p| p.to_string_lossy().replace('\\', "/"));
        std::thread::spawn(move || {
            let mut guard = match slot.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if guard.is_some() {
                return; // already warm
            }
            if let Ok(mut engine) = start_engine(&root) {
                match warm_file.and_then(|f| std::fs::read_to_string(root.join(&f)).ok().map(|c| (f, c))) {
                    Some(file) => {
                        let _ = engine.diagnostics(&[file]); // forces the project to load
                    }
                    None => {
                        let _ = engine.diagnostics(&[]);
                    }
                }
                *guard = Some(engine);
            }
        });
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        // Read structure from the loaded SCIP index; gate via the PERSISTENT write engine
        // (VFS overlay + baseline-diff diagnostics over the blast radius). Reuse the warm
        // engine from `prewarm` — locking blocks until an in-flight warm finishes, so we
        // never start a second cold engine. Only start fresh if prewarm never ran or failed.
        let timing = std::env::var("CI_TIMING").is_ok();
        let t0 = std::time::Instant::now();
        let mut guard = self.engine.lock().map_err(|_| Error::Driver("engine lock poisoned".into()))?;
        if guard.is_none() {
            *guard = Some(start_engine(&self.root)?);
        }
        let engine: &mut dyn GateEngine = guard.as_mut().unwrap().as_mut();
        if timing {
            eprintln!("[timing] engine ready (warm or fresh) {:?}", t0.elapsed());
        }
        let t1 = std::time::Instant::now();
        // Resolve anchors from the FULL structure (SCIP + tree-sitter sub-nodes), NOT raw SCIP —
        // otherwise sub-node targets (`:body`/`:return`/`:param.N`) that `list_anchors` advertises
        // can't be found here, and `set_body` / `replace_node target:…` reject with "anchor not
        // found". Must match what `structure()` returns to the agent.
        let structure_of = |f: &str| self.structure(Path::new(f)).unwrap_or_default();

        // Reverse import map (file -> who imports it) for the delete-safety check — from the
        // OVERLAID graph, so edges added/removed by earlier edits in this session count.
        let reverse = ci_core::reverse_import_map(&self.import_graph().unwrap_or_default());
        let reverse_imports = |file: &str| reverse.get(file).cloned().unwrap_or_default();

        let r = ci_edit::commit_edits(&self.root, ops, &structure_of, engine, opts, &reverse_imports);
        if timing {
            eprintln!("[timing] commit_edits (warmup+rename+gate) {:?}", t1.elapsed());
        }
        // Keep reads true in-session: have the engine re-describe the committed files (new
        // symbols, new import edges) and stash the result as read overrides. Best-effort — a
        // refresh hiccup must NOT fail the (already-committed) edit; reads then lag until the
        // next startup reindex, exactly as before this hook existed.
        if let Ok(CommitResult::Ok { changed_files, .. }) = &r {
            if opts.write && !opts.dry_run && !changed_files.is_empty() {
                let rels: Vec<String> =
                    changed_files.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
                match engine.file_summaries(&rels) {
                    Ok(Some(summaries)) => {
                        if let Ok(mut m) = self.fresh.lock() {
                            for s in summaries {
                                m.insert(s.path.clone(), s);
                            }
                        }
                    }
                    Ok(None) => {} // engine can't re-describe (LSP fallback): reads lag until reindex
                    Err(e) => eprintln!("[lang-ts] post-edit read refresh failed ({e}); structure/import_graph lag until the next reindex"),
                }
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // The post-edit read override: a `fresh` entry must win over the (empty) SCIP index for
    // structure() — including tree-sitter deepening from current disk content — and rewrite
    // that file's outgoing import edges; a deleted entry must blank both.
    #[test]
    fn fresh_overrides_shadow_the_scip_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".codeindex")).unwrap();
        let idx = root.join(".codeindex/index.scip");
        fs::write(&idx, b"").unwrap(); // valid, empty SCIP index
        fs::write(root.join("a.ts"), "export function add(a: number): number {\n  return a;\n}\n").unwrap();

        let p = TsProvider::from_index(root, &idx).unwrap();
        assert!(p.structure(Path::new("a.ts")).unwrap().is_empty(), "empty index, no override yet");

        let node = Node {
            id: "a.ts#add".into(),
            name: Some("add".into()),
            kind: ci_core::NodeKind::Symbol(ci_core::SymbolKind::Function),
            range: ci_core::Range { start_line: 1, start_char: 0, end_line: 3, end_char: 1 },
            name_range: Some(ci_core::Range { start_line: 1, start_char: 16, end_line: 1, end_char: 19 }),
            children: vec![],
        };
        p.fresh.lock().unwrap().insert(
            "a.ts".into(),
            FileSummary { path: "a.ts".into(), deleted: false, nodes: vec![node], imports: vec![PathBuf::from("b.ts")] },
        );

        let nodes = p.structure(Path::new("a.ts")).unwrap();
        let add = nodes.iter().find(|n| n.id == "a.ts#add").expect("override symbol served");
        assert!(add.children.iter().any(|c| c.id == "a.ts#add:body"), "override still deepened: {:?}", add.children);
        let g = p.import_graph().unwrap();
        assert_eq!(g.get(&PathBuf::from("a.ts")).unwrap(), &vec![PathBuf::from("b.ts")], "override edge served");

        p.fresh.lock().unwrap().insert(
            "a.ts".into(),
            FileSummary { path: "a.ts".into(), deleted: true, nodes: vec![], imports: vec![] },
        );
        assert!(p.structure(Path::new("a.ts")).unwrap().is_empty(), "deleted override blanks structure");
        assert!(!p.import_graph().unwrap().contains_key(&PathBuf::from("a.ts")), "deleted override removes edges");
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
        let scip = root.join(".codeindex/index.scip");
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
}
