//! lang-ts — the TypeScript [`LanguageProvider`]. v1 read path: run
//! `scip-typescript` (via `npx`, no global install) to produce `index.scip`, then
//! serve `structure()` + `import_graph()` from [`ScipIndex`]. The write path
//! (VFS + LSP gate) lands in P2.
use ci_core::{
    CommitResult, EditOp, EditOpts, Error, Granularity, ImportGraph, LanguageProvider, Node, Result,
};
use ci_edit::GateEngine;
use ci_scip::ScipIndex;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

mod ast;
mod outline;
mod tsmorph;

pub use outline::outline;

/// Fresh npm cache dir so a corrupted default `~/.npm` cache can't break `npx`.
fn npm_cache() -> PathBuf {
    std::env::var("CI_NPM_CACHE").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("ci-npm-cache"))
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
    /// Index `root` with scip-typescript (`npx @sourcegraph/scip-typescript`), then load it.
    pub fn index(root: &Path) -> Result<Self> {
        let out = root.join(".codeindex").join("index.scip");
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let status = Command::new("npx")
            .args([
                "--yes",
                "@sourcegraph/scip-typescript",
                "index",
                "--infer-tsconfig",
                "--no-progress-bar",
                "--output",
            ])
            .arg(&out)
            .current_dir(root)
            .env("npm_config_cache", npm_cache())
            // Discard the indexer's stdout — it must never pollute an MCP/JSON-RPC stream.
            .stdout(Stdio::null())
            .status()
            .map_err(|e| Error::Driver(format!("launching scip-typescript via npx failed: {e}")))?;
        if !status.success() {
            return Err(Error::Driver(format!("scip-typescript index failed ({status})")));
        }
        Self::from_index(root, &out)
    }

    /// Load a provider from an existing `index.scip` (skip running the indexer).
    pub fn from_index(root: &Path, index_scip: &Path) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            scip: Arc::new(ScipIndex::load(index_scip)?),
            engine: Arc::new(Mutex::new(None)),
        })
    }

    /// Start the write engine and load the project NOW, on a background thread, so the first
    /// `apply_edits` finds it warm instead of paying the ~seconds cold project load inline.
    /// The thread holds the engine lock for the duration, so an `apply_edits` that arrives
    /// mid-warm simply waits for it rather than racing in a second cold engine. Safe no-op if
    /// the engine can't start (apply_edits falls back to starting one fresh).
    pub fn prewarm(&self) {
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

    /// The TS language-server command (npx tsls). All external/Node tooling lives
    /// here in the provider — the core + ci-lsp stay pure Rust.
    fn ts_lsp_command() -> Command {
        let mut c = Command::new("npx");
        c.args(["--yes", "-p", "typescript-language-server", "-p", "typescript", "typescript-language-server", "--stdio"])
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
        let scip_nodes = self.scip.structure(&rel)?;
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
        self.scip.import_graph()
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
        let structure_of = |f: &str| self.scip.structure(f).unwrap_or_default();

        // Reverse import map (file -> who imports it) for the delete-safety check.
        let mut reverse: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for (from, tos) in self.scip.import_graph().unwrap_or_default() {
            let f = from.to_string_lossy().replace('\\', "/");
            for to in tos {
                reverse.entry(to.to_string_lossy().replace('\\', "/")).or_default().push(f.clone());
            }
        }
        let reverse_imports = |file: &str| reverse.get(file).cloned().unwrap_or_default();

        let r = ci_edit::commit_edits(&self.root, ops, &structure_of, engine, opts, &reverse_imports);
        if timing {
            eprintln!("[timing] commit_edits (warmup+rename+gate) {:?}", t1.elapsed());
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    }
}
