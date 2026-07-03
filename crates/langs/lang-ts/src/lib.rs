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

/// Fresh npm cache dir so a corrupted default `~/.npm` cache can't break `npx`. Shared with the
/// ts-morph sidecar (`tsmorph.rs`) so both TS tooling paths use the same cache location.
pub(crate) fn npm_cache() -> PathBuf {
    std::env::var("CI_NPM_CACHE").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("ci-npm-cache"))
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
/// Engine preference: **tsgo → ts-morph → tsls**. tsgo (the TS7 native LSP) gates ~138x
/// faster warm than the alternatives with identical verdicts (docs/benchmarks.md), but is
/// auto-picked only when it needs NO network (`CI_TSGO`, or `tsgo` on PATH) — a surprise npx
/// download doesn't belong in the middle of someone's first edit. `CI_EDIT_ENGINE` forces one
/// tier: `tsgo` | `tsmorph` | `lsp` (tsls, or whatever `CI_TS_LSP_SERVER` names).
fn start_engine(root: &Path) -> Result<Box<dyn GateEngine + Send>> {
    let pref = std::env::var("CI_EDIT_ENGINE").unwrap_or_default();
    match pref.as_str() {
        "tsgo" => return Ok(Box::new(ci_lsp::LspClient::start(root, TsProvider::tsgo_lsp_command())?)),
        "lsp" => return start_tsls(root),
        _ => {}
    }
    if pref.is_empty() {
        if let Some(bin) = local_tsgo() {
            let mut c = Command::new(bin);
            c.args(["--lsp", "-stdio"]);
            match ci_lsp::LspClient::start(root, c) {
                Ok(client) => return Ok(Box::new(client)),
                Err(e) => eprintln!("[lang-ts] local tsgo failed to start ({e}); falling back to ts-morph"),
            }
        }
    }
    match tsmorph::TsMorphClient::start(root) {
        Ok(c) => return Ok(Box::new(c)),
        Err(e) if pref == "tsmorph" => return Err(e), // forced: surface the failure
        Err(_) => {} // auto: fall back to LSP
    }
    start_tsls(root)
}

/// The tsls fallback tier, with the toolchain-aware error message.
fn start_tsls(root: &Path) -> Result<Box<dyn GateEngine + Send>> {
    match ci_lsp::LspClient::start(root, TsProvider::ts_lsp_command()) {
        Ok(c) => Ok(Box::new(c)),
        // Both engines need Node; when the toolchain itself is the problem, say THAT (with the
        // install hint) instead of a raw spawn error.
        Err(e) => match toolchain().describe_missing() {
            Some(missing) => Err(Error::Driver(format!("TypeScript edit engine failed to start ({e}).\n{missing}"))),
            None => Err(e),
        },
    }
}

/// A tsgo binary that costs nothing to use: `CI_TSGO`, else a `tsgo` on PATH. `None` means
/// the auto tier skips tsgo (explicit `CI_EDIT_ENGINE=tsgo` may still fetch via npx).
fn local_tsgo() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CI_TSGO") {
        return Some(PathBuf::from(bin));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("tsgo")).find(|p| p.is_file())
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
        for doc in provider.scip.documents() {
            if let std::collections::btree_map::Entry::Vacant(slot) = fp.entry(doc) {
                if let Some(h) = hash_file(&root.join(slot.key())) {
                    slot.insert(h);
                }
            }
        }
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
        let bytes = ci_lsp_index::sweep_index(root, &files, Self::tsgo_lsp_command(), "lspx-ts")?;
        let out = root.join(".marksman").join("index.lspx.scip");
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&out, bytes)?;
        Self::from_index(root, &out)
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

    /// The tsgo (TypeScript 7 native) LSP command for the sweep indexer. `CI_TSGO` points at
    /// a tsgo binary directly; the default fetches the native preview via npx (the `typescript`
    /// RC npm package ships only `tsc` — the LSP binary lives in `@typescript/native-preview`).
    fn tsgo_lsp_command() -> Command {
        if let Ok(bin) = std::env::var("CI_TSGO") {
            let mut c = Command::new(bin);
            c.args(["--lsp", "-stdio"]);
            return c;
        }
        let mut c = Command::new("npx");
        c.arg("--yes")
            .arg("-p")
            .arg("@typescript/native-preview")
            .args(["tsgo", "--lsp", "-stdio"])
            .env("npm_config_cache", npm_cache());
        c
    }

    /// The TS language-server command (npx tsls). All external/Node tooling lives
    /// here in the provider — the core + ci-lsp stay pure Rust.
    ///
    /// `CI_TS_LSP_SERVER` overrides the whole command line (whitespace-split, no quoting) —
    /// e.g. `".../node_modules/.bin/tsgo --lsp -stdio"` runs the TS7 native-port server.
    fn ts_lsp_command() -> Command {
        if let Ok(raw) = std::env::var("CI_TS_LSP_SERVER") {
            let mut parts = raw.split_whitespace();
            if let Some(prog) = parts.next() {
                let mut c = Command::new(prog);
                c.args(parts);
                return c;
            }
        }
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
                    Ok(None) => {
                        // LSP engines (tsgo/tsls) can't re-describe their live project the way
                        // the ts-morph sidecar does. Approximate from tree-sitter on CURRENT
                        // disk — the same read shape TsTreeGated serves — so reads track the
                        // commit instead of serving pre-edit state; scip fidelity (flattened
                        // barrels, semantic edges) returns at the next reindex.
                        let fb = lang_fallback::FallbackProvider::new(&self.root, lang_fallback::FbLang::Ts);
                        let graph = fb.import_graph().unwrap_or_default();
                        if let Ok(mut m) = self.fresh.lock() {
                            for rel in &rels {
                                let deleted = !self.root.join(rel).exists();
                                let nodes = if deleted {
                                    vec![]
                                } else {
                                    fb.structure(Path::new(rel)).unwrap_or_default()
                                };
                                let imports = graph.get(&PathBuf::from(rel)).cloned().unwrap_or_default();
                                m.insert(
                                    rel.clone(),
                                    FileSummary { path: rel.clone(), deleted, nodes, imports },
                                );
                            }
                        }
                    }
                    Err(e) => eprintln!("[lang-ts] post-edit read refresh failed ({e}); structure/import_graph lag until the next reindex"),
                }
            }
        }
        r
    }
}

// ── the CI_TS_MODE=treesitter-gated ablation provider ────────────────────────

/// TypeScript with a tree-sitter READ path (the generic fallback's TS collector + its
/// relative-import graph — no scip, no index build, no Node at startup) and the SAME warm
/// ts-morph GATE as the full provider. Exists to measure end to end what SCIP's
/// compiler-accurate symbols and reference graph actually buy (see docs/benchmarks.md); the
/// registry builders construct it only under `CI_TS_MODE=treesitter-gated`. Note ts-morph's
/// `rename` is still project-wide (the compiler finds references) — the ablated piece is the
/// read/blast-radius fidelity, not the rename.
#[derive(Clone)]
pub struct TsTreeGated {
    root: PathBuf,
    read: lang_fallback::FallbackProvider,
    engine: WarmEngine,
}

impl TsTreeGated {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            read: lang_fallback::FallbackProvider::new(root, lang_fallback::FbLang::Ts),
            engine: Arc::new(Mutex::new(None)),
        }
    }
}

impl LanguageProvider for TsTreeGated {
    fn granularity(&self) -> Granularity {
        Granularity::Ast
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        self.read.structure(file)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        self.read.import_graph()
    }

    fn prewarm(&self) {
        let slot = self.engine.clone();
        let root = self.root.clone();
        std::thread::spawn(move || {
            let Ok(mut guard) = slot.lock() else { return };
            if guard.is_some() {
                return;
            }
            if let Ok(mut engine) = start_engine(&root) {
                let _ = engine.diagnostics(&[]);
                *guard = Some(engine);
            }
        });
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        let mut guard = self.engine.lock().map_err(|_| Error::Driver("engine lock poisoned".into()))?;
        if guard.is_none() {
            *guard = Some(start_engine(&self.root)?);
        }
        let engine: &mut dyn GateEngine = guard.as_mut().unwrap().as_mut();
        let structure_of = |f: &str| self.read.structure(Path::new(f)).unwrap_or_default();
        // Blast radius from the tree-sitter relative-import graph. Syntactic edges do NOT
        // flatten barrels — a consumer of `export * from './x'` edges to the barrel, not x —
        // so the gate's one hop must be served the TRANSITIVE reverse-importer set or a barrel
        // hides its consumers and the gate claims a false "clean" (measured: bench T9-barrel).
        // Full/scip mode keeps one-hop: its semantic graph is already flattened.
        let reverse = ci_core::reverse_import_map(&self.read.import_graph().unwrap_or_default());
        let reverse_imports = |file: &str| ci_core::transitive_reverse_imports(&reverse, file);
        ci_edit::commit_edits(&self.root, ops, &structure_of, engine, opts, &reverse_imports)
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
        fs::create_dir_all(root.join(".marksman")).unwrap();
        let idx = root.join(".marksman/index.scip");
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

        let run_with_engine = |engine: &str| -> (String, String) {
            let saved = std::env::var("CI_EDIT_ENGINE").ok();
            std::env::set_var("CI_EDIT_ENGINE", engine);
            let dir = tempfile::tempdir().unwrap();
            write_fixture(dir.path());
            let p = TsProvider::index(dir.path()).expect("scip-typescript indexing");
            let res = p.apply_edits(std::slice::from_ref(&rename), &opts).unwrap();
            match saved {
                Some(v) => std::env::set_var("CI_EDIT_ENGINE", v),
                None => std::env::remove_var("CI_EDIT_ENGINE"),
            }
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
