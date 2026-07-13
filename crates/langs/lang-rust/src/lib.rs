//! lang-rust — the Rust [`LanguageProvider`]. v0 read path: in-process `tree-sitter-rust`
//! (no external tooling — Rust's parser is a Rust crate) for `structure()` (items + fn
//! sub-nodes) and `import_graph()` (`mod` resolution). Compiler-accurate references and
//! type-checked edits via rust-analyzer are on the roadmap; this is what lets Marksman
//! index and retrieve Rust — including its own source — today.
use ci_core::{
    CommitResult, EditOp, EditOpts, Error, FileSummary, Granularity, ImportGraph,
    LanguageProvider, Node, ReadIndex, Result, SymbolKind,
};
use ci_edit::{Composed, GateEngine};
use ci_lsp::LspClient;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tree_sitter::Parser;

mod gate;
mod graph;
mod movefix;
mod structure;

use gate::RustEngine;

/// The Rust provider, assembled from its two halves — [`RustRead`] (the read index the
/// agent plans against) × [`gate::RustEngine`] (rust-analyzer rename/move + the `cargo
/// check` verdict) — glued by [`Composed`]: post-commit graph freshness and the blast-radius
/// policy are derived from the reader's advertised properties (`live`/`live_graph`/
/// `semantic_edges`), never hand-wired here. What stays Rust-specific in this crate: the
/// grammar hooks (structure/graph/movefix), the toolchain probes, and the `pub mod x;`
/// synthesis on `create_file` (an orphan `.rs` file never compiles).
#[derive(Clone)]
pub struct RustProvider {
    root: PathBuf,
    inner: Arc<Composed<RustRead>>,
}

/// The Rust READ half ([`ReadIndex`]): `structure()` is a LIVE tree-sitter parse of current
/// disk; `import_graph()` serves the cached `rust-analyzer scip` graph when enabled and
/// trustworthy, else the instant syntactic `mod`+`use` graph. Hence the hybrid contract it
/// advertises: `live` (structure re-reads disk), `live_graph`/`semantic_edges` following
/// whether the scip artifact is actually being served — the glue then overlays post-commit
/// edges on the artifact graph and sizes the blast radius (one-hop scip, transitive
/// syntactic) to match.
#[derive(Clone)]
struct RustRead {
    root: PathBuf,
    /// Use the cached `rust-analyzer scip` graph (compiler-accurate `use` edges) over the
    /// tree-sitter `mod` graph. Set by the caller from `Config::scip_enabled("rust")`.
    use_scip: bool,
    /// The scip cache's base graph, loaded + drift-checked ONCE per provider (`None` inside =
    /// cache unusable: absent, unreadable, or no fingerprint to trust it by). Files that
    /// drifted since the cache was built have their edges re-described from tree-sitter and
    /// baked in AT LOAD — read-side truth the glue never sees; post-commit freshness is the
    /// glue's job.
    scip_base: Arc<Mutex<Option<Option<ImportGraph>>>>,
}

/// What ONE bare `move_file` covers for Rust — composed into the MCP `apply_edits`
/// description by ci-mcp, so the completeness claim the agent reads lives NEXT TO the code
/// that makes it true (movefix + the gate) and a new language's claim appears with its
/// provider instead of drifting in hand-written prose. Keep it one sentence fragment.
pub const MOVE_COVERAGE: &str = "the `mod` declaration (moved/repurposed), a parent `mod.rs` CREATED when the target directory needs one, and every `crate::…` path rewritten";

/// The rust-analyzer binary: `$CI_RUST_ANALYZER`, else `~/.cargo/bin/rust-analyzer`, else PATH.
fn rust_analyzer_command() -> Command {
    let bin = std::env::var("CI_RUST_ANALYZER").map(std::ffi::OsString::from).unwrap_or_else(|_| {
        std::env::var("HOME")
            .ok()
            .map(|h| Path::new(&h).join(".cargo/bin/rust-analyzer"))
            .filter(|p| p.is_file())
            .map(|p| p.into_os_string())
            .unwrap_or_else(|| "rust-analyzer".into())
    });
    Command::new(bin)
}

/// What the Rust provider needs from the machine. Honest scoping: the READ path (structure,
/// import graph) is in-process tree-sitter and needs NOTHING external — only type-checked
/// edits (the gate, rename/move) need rust-analyzer. The registry builders surface this at
/// startup; `apply_edits` repeats it if the engine actually fails to spawn.
pub fn toolchain() -> ci_core::ToolchainReport {
    ci_core::ToolchainReport {
        lang: "rust",
        tools: vec![ci_core::ToolStatus {
            tool: "rust-analyzer",
            needed_for: "type-checked edits (rename / gate); reads work without it",
            install: "`rustup component add rust-analyzer` — or point CI_RUST_ANALYZER at the binary",
            found: ci_core::probe_tool(rust_analyzer_command().arg("--version")),
        }],
    }
}

impl RustProvider {
    pub fn new(root: &Path) -> Self {
        Self::assemble(root, false)
    }

    /// TS-parity startup (`TsProvider::open` is the model): when the semantic graph is
    /// enabled and its artifact is MISSING, generate it on first open (≈ a `cargo check`,
    /// exactly like TypeScript's first scip-typescript run); afterwards the cached graph
    /// loads instantly and drift is overlaid per file — startup never regenerates. Reads are
    /// never blocked: a failed generation warns and serves the tree-sitter graph.
    pub fn open(root: &Path, use_scip: bool) -> Self {
        if use_scip && !root.join(".marksman").join("rust.scip").is_file() {
            eprintln!("[lang-rust] generating rust-analyzer scip graph (first open, ≈ cargo check) …");
            if let Err(e) = refresh_scip(root) {
                eprintln!("[lang-rust] scip graph unavailable ({e}); using the tree-sitter graph");
            }
        }
        Self::new(root).with_scip(use_scip)
    }

    /// Enable the compiler-accurate `rust-analyzer scip` graph (see [`RustRead::use_scip`]).
    pub fn with_scip(self, use_scip: bool) -> Self {
        Self::assemble(&self.root, use_scip)
    }

    /// Wire the halves: the read index advertises its properties, the engine factory carries
    /// the toolchain hint, the prewarmer issues rust-analyzer's real warming call, and the
    /// live summarizer re-describes committed files from tree-sitter for the glue's
    /// post-commit graph overlay.
    fn assemble(root: &Path, use_scip: bool) -> Self {
        let read = RustRead::new(root, use_scip);
        let summarize_root = root.to_path_buf();
        let inner = Composed::new(root, read, engine_factory())
            .with_live_summarizer(Arc::new(move |rel| file_summary(&summarize_root, rel)))
            .with_prewarmer(Arc::new(prewarm_engine));
        Self { root: root.to_path_buf(), inner: Arc::new(inner) }
    }

    pub(crate) fn parse(content: &str) -> Option<tree_sitter::Tree> {
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).ok()?;
        parser.parse(content, None)
    }
}

/// Builds the write engine (lazily in `apply_edits`, or via the prewarmer). When the
/// toolchain itself is the problem, say THAT (with the install hint) instead of a raw spawn
/// error — reads worked fine, so this is the user's first signal that the WRITE path has a
/// missing dependency.
fn engine_factory() -> ci_edit::EngineFactory {
    Arc::new(|root: &Path| {
        let sandbox = ci_core::resolve_sandbox(root, "marksman-rust");
        let ra = ci_core::tool_command(&*sandbox, "rust-analyzer", || Ok(rust_analyzer_command()))?;
        let lsp = LspClient::start_in(root, ra, &*sandbox).map_err(|e| {
            match toolchain().describe_missing() {
                Some(missing) => Error::Driver(format!("rust edit engine failed to start ({e}).\n{missing}")),
                None => e,
            }
        })?;
        Ok(Box::new(RustEngine { root: root.to_path_buf(), lsp, sandbox }) as Box<dyn GateEngine + Send>)
    })
}

/// The rust-analyzer warming recipe ([`ci_edit::Prewarmer`]): start the LSP and pull one real
/// file's diagnostics — that call is what forces the cold `cargo metadata` + analysis load.
/// It must hit the RAW LSP client, not [`RustEngine`] (whose `diagnostics` is the cargo
/// gate). No-op-safe: `None` if rust-analyzer can't start (apply_edits then surfaces the
/// error).
fn prewarm_engine(root: &Path) -> Option<Box<dyn GateEngine + Send>> {
    let warm = graph::rust_files(root)
        .into_iter()
        .find_map(|rel| std::fs::read_to_string(root.join(&rel)).ok().map(|c| (rel, c)));
    let sandbox = ci_core::resolve_sandbox(root, "marksman-rust");
    let ra = ci_core::tool_command(&*sandbox, "rust-analyzer", || Ok(rust_analyzer_command())).ok()?;
    let mut client = LspClient::start_in(root, ra, &*sandbox).ok()?;
    if let Some((f, content)) = warm {
        let _ = client.diagnostics(&[(f, content)]); // forces the workspace to load
    }
    Some(Box::new(RustEngine { root: root.to_path_buf(), lsp: client, sandbox }))
}

/// Re-describe one committed file for the glue's post-commit graph overlay (tree-sitter,
/// in-process — cheap and can't fail the edit): current-disk symbols + outgoing edges, a
/// `deleted` summary when the file is gone (its graph entry must drop), `None` for non-`.rs`
/// files (manifests never carry edges). A parse failure yields no imports — the entry drops,
/// matching what serving edges-from-disk did for an unreadable parse.
fn file_summary(root: &Path, rel: &str) -> Option<FileSummary> {
    if !rel.ends_with(".rs") {
        return None;
    }
    let Ok(content) = std::fs::read_to_string(root.join(rel)) else {
        return Some(FileSummary { path: rel.into(), deleted: true, nodes: vec![], imports: vec![] });
    };
    let (nodes, imports) = match RustProvider::parse(&content) {
        Some(tree) => {
            let bytes = content.as_bytes();
            let mut nodes = Vec::new();
            structure::collect_items(tree.root_node(), bytes, &format!("{rel}#"), SymbolKind::Function, &mut nodes);
            (nodes, graph::file_edges(root, rel, tree.root_node(), bytes))
        }
        None => (Vec::new(), Vec::new()),
    };
    Some(FileSummary { path: rel.into(), deleted: false, nodes, imports })
}

impl RustRead {
    fn new(root: &Path, use_scip: bool) -> Self {
        Self { root: root.to_path_buf(), use_scip, scip_base: Arc::new(Mutex::new(None)) }
    }

    /// The cached `rust-analyzer scip` index (the optional compiler-accurate graph source).
    fn scip_cache(&self) -> PathBuf {
        self.root.join(".marksman").join("rust.scip")
    }

    /// The `use`/reference import graph from the cached SCIP index, kept honest: the base
    /// graph is loaded once and trusted only as far as its fingerprint reaches — every file
    /// that drifted since `refresh_scip` ran gets its edges recomputed from tree-sitter
    /// (`mod` + resolved `use` paths) and baked in at load. `None` (→ the `mod`-graph
    /// fallback) when there is no cache, or a cache with no fingerprint at all — a graph we
    /// can't vouch for is never served.
    fn scip_graph(&self) -> Option<ImportGraph> {
        let mut base_slot = self.scip_base.lock().ok()?;
        if base_slot.is_none() {
            *base_slot = Some(self.load_scip_base());
        }
        base_slot.as_ref()?.clone()
    }

    /// Whether [`scip_graph`](RustRead::scip_graph) would serve (loading it on first ask) —
    /// the fact `live_graph`/`semantic_edges` advertise, without cloning the graph.
    fn serving_scip(&self) -> bool {
        let Ok(mut base_slot) = self.scip_base.lock() else { return false };
        if base_slot.is_none() {
            *base_slot = Some(self.load_scip_base());
        }
        matches!(base_slot.as_ref(), Some(Some(_)))
    }

    /// Load + drift-check the scip cache (once per provider). Files that changed since the
    /// cache was built get their edges re-described from disk and baked into the base.
    fn load_scip_base(&self) -> Option<ImportGraph> {
        let cache = self.scip_cache();
        if !cache.is_file() {
            return None;
        }
        let graph = ci_scip::ScipIndex::load(&cache).ok()?.import_graph().ok()?;
        let Some(drift) = graph::drifted_files(&self.root) else {
            eprintln!(
                "[lang-rust] scip cache {} has no fingerprint — refusing to serve a graph of unknown \
                 age (falling back to the mod graph); re-run `index` to regenerate it",
                cache.display()
            );
            return None;
        };
        if drift.is_empty() {
            return Some(graph);
        }
        let mut seed: HashMap<String, Option<Vec<PathBuf>>> = HashMap::new();
        for rel in &drift {
            if rel.ends_with(".rs") {
                seed.insert(rel.clone(), self.edges_from_disk(rel));
            }
        }
        eprintln!(
            "[lang-rust] scip graph: {} file(s) changed since the cache was built; serving \
             tree-sitter edges for those files (scip edges for the rest)",
            drift.len()
        );
        Some(graph::overlay_graph(graph, &seed))
    }

    /// The instant in-process graph: tree-sitter `mod` + resolved `use` edges over the whole
    /// repo — the SAME per-file machinery the scip drift overlay uses. It must include `use`
    /// edges: a file's importers via `use crate::x::…` are exactly the blast radius a
    /// move/delete needs, and the `mod`-only graph left them out (a moved module's consumers
    /// were invisible to the gate when no scip cache existed — bench move-rust round 4).
    fn syntactic_graph(&self) -> ImportGraph {
        let mut graph: ImportGraph = BTreeMap::new();
        for rel in graph::rust_files(&self.root) {
            if let Some(edges) = self.edges_from_disk(&rel) {
                if !edges.is_empty() {
                    graph.insert(PathBuf::from(&rel), edges);
                }
            }
        }
        graph
    }

    /// Current outgoing edges of `rel` from disk (`None` = file gone).
    fn edges_from_disk(&self, rel: &str) -> Option<Vec<PathBuf>> {
        let content = std::fs::read_to_string(self.root.join(rel)).ok()?;
        let tree = RustProvider::parse(&content)?;
        Some(graph::file_edges(&self.root, rel, tree.root_node(), content.as_bytes()))
    }
}

impl ReadIndex for RustRead {
    fn granularity(&self) -> Granularity {
        Granularity::Ast // tree-sitter sub-nodes (params / return / body)
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        // Read jail (twin of ci-edit's write jail): an out-of-root path has
        // no nodes — never a read outside the registered workspace.
        let Some(rel) = ci_core::jailed_rel(&self.root, file) else {
            return Ok(vec![]);
        };
        let content = match std::fs::read_to_string(self.root.join(&rel)) {
            Ok(c) => c,
            Err(_) => return Ok(vec![]),
        };
        let Some(tree) = RustProvider::parse(&content) else { return Ok(vec![]) };
        let bytes = content.as_bytes();
        let mut out = Vec::new();
        let prefix = format!("{rel}#");
        structure::collect_items(tree.root_node(), bytes, &prefix, SymbolKind::Function, &mut out);
        Ok(out)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        // OPT-IN (`use_scip`, from config/env): a compiler-accurate `use`/reference graph from a
        // cached `rust-analyzer scip` index — far richer than `mod` edges. Read-only here
        // (generation is `refresh_scip`, run at index time), and we fall back to the instant
        // tree-sitter `mod` graph whenever the cache is absent, so this never blocks the live path.
        if self.use_scip {
            if let Some(g) = self.scip_graph() {
                return Ok(g);
            }
        }
        Ok(self.syntactic_graph())
    }

    /// Structure re-parses current disk on every read — no fresh-summary override may ever
    /// shadow it.
    fn live(&self) -> bool {
        true
    }

    /// The served graph is an artifact exactly when the scip cache is trusted; the glue then
    /// overlays committed edits' edges so the graph never reports pre-edit edges. The
    /// syntactic graph re-reads disk per call and needs no overlay.
    fn live_graph(&self) -> bool {
        !(self.use_scip && self.serving_scip())
    }

    /// Blast-radius policy follows the graph's edge semantics (bench T9): the
    /// compiler-accurate scip `use` graph flattens re-exports, so ONE reverse hop is sound;
    /// the syntactic fallback does not, so its radius must expand TRANSITIVELY or a
    /// `pub use` chain hides consumers from the gate.
    fn semantic_edges(&self) -> bool {
        self.use_scip && self.serving_scip()
    }
}

/// Generate the cached SCIP index (`<root>/.marksman/rust.scip`) by running
/// `rust-analyzer scip` — the source for the optional compiler-accurate `use` graph
/// (`CI_RUST_SCIP`). Run at index time (a batch step); `import_graph` then reads it. Slow (≈ a
/// `cargo check`), so it's never on the live path. Errors propagate so the caller can warn and
/// fall back to the tree-sitter `mod` graph.
/// `refresh_scip`, skipped when the cache is already true to the source (fingerprint match,
/// zero drifted files) — `index` calls this so a fresh `open()`-generated cache isn't paid
/// for twice, while a stale one is regenerated at the batch step where slow is acceptable.
pub fn refresh_scip_if_stale(root: &Path) -> Result<bool> {
    if root.join(".marksman").join("rust.scip").is_file() {
        if let Some(drift) = graph::drifted_files(root) {
            if drift.is_empty() {
                return Ok(false); // cache fresh — nothing to do
            }
        }
    }
    refresh_scip(root)?;
    Ok(true)
}

pub fn refresh_scip(root: &Path) -> Result<()> {
    let out = root.join(".marksman").join("rust.scip");
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).map_err(|e| Error::Driver(format!("scip cache dir: {e}")))?;
    }
    let status = rust_analyzer_command()
        .arg("scip")
        .arg(".")
        .arg("--output")
        .arg(&out)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| Error::Driver(format!("launching `rust-analyzer scip`: {e}")))?;
    if !status.success() {
        return Err(Error::Driver(format!("`rust-analyzer scip` failed ({status})")));
    }
    // The fingerprint is what lets a later session TRUST this cache (and pinpoint which files
    // drifted). Without one the graph falls back to mod edges, so failing to write it is an
    // error, not a shrug.
    graph::store_fingerprint(root)
        .map_err(|e| Error::Driver(format!("storing the scip cache fingerprint: {e}")))?;
    Ok(())
}

impl LanguageProvider for RustProvider {
    fn granularity(&self) -> Granularity {
        self.inner.granularity()
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        self.inner.structure(file)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        self.inner.import_graph()
    }

    /// Start rust-analyzer and load the cargo workspace NOW, on a background thread, so the
    /// first `apply_edits` finds it warm instead of paying the cold `cargo metadata` + analysis
    /// inline. The recipe is [`prewarm_engine`]; the lock/wait/no-double-start discipline
    /// lives in `ci_edit::spawn_prewarm`, driven by the glue.
    fn prewarm(&self) {
        self.inner.prewarm()
    }

    /// The write path is [`Composed`]: persistent rust-analyzer engine (reuse from prewarm),
    /// the same VFS + baseline-diff + blast-radius spine as TypeScript, radius policy from
    /// the read half's edge semantics, post-commit graph freshness from the live summarizer.
    /// The one Rust-specific step happens HERE, before the glue sees the batch:
    /// create_file of an UNDECLARED module file synthesizes its `pub mod x;` declaration
    /// right after the create (movefix::declare_module_edit) — an orphan .rs file never
    /// compiles, so every agent hand-writes this edit; server-side it is deterministic.
    /// Skipped when any batch op already touches the parent decl file (the agent is
    /// handling membership itself — synthesizing too would DUPLICATE the declaration).
    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        let mut expanded: Vec<EditOp> = Vec::with_capacity(ops.len() + 1);
        for op in ops {
            let synth = if let EditOp::CreateFile { path, .. } = op {
                let rel = path.to_string_lossy().replace('\\', "/");
                movefix::declare_module_edit(&self.root, &rel).filter(|(parent, _, _)| {
                    !ops.iter().any(|o| {
                        ci_edit::op_touches_file(o, parent) && !matches!(o, EditOp::CreateFile { .. })
                    })
                })
            } else {
                None
            };
            expanded.push(op.clone());
            if let Some((parent, old_text, new_text)) = synth {
                expanded.push(EditOp::ReplaceInFile { path: parent.into(), old_text, new_text });
            }
        }
        self.inner.apply_edits(&expanded, opts)
    }
}

// ── skeletal context ─────────────────────────────────────────────────────────

/// Return `content` with Rust function/method bodies (`block`) elided, keeping signatures.
/// Best-effort: returns the original on a parse failure.
pub fn outline(content: &str) -> String {
    // Fold each `function_item`'s `block` body; keep everything else (signatures, types).
    ci_treesitter::outline(&tree_sitter_rust::LANGUAGE.into(), content, &["function_item"], &["block"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_core::NodeKind;
    use std::fs;

    #[test]
    fn structure_extracts_items_and_methods() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.rs"),
            "pub struct Foo { x: i32 }\nimpl Foo {\n  pub fn bar(&self, n: i32) -> i32 { n + self.x }\n}\nfn top() {}\n",
        )
        .unwrap();
        let p = RustProvider::new(root);
        let nodes = p.structure(Path::new("a.rs")).unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"a.rs#Foo"), "struct: {ids:?}");
        assert!(ids.contains(&"a.rs#Foo.bar"), "method qualified by impl type: {ids:?}");
        assert!(ids.contains(&"a.rs#top"), "top-level fn: {ids:?}");
        // the method carries params/return/body sub-nodes
        let bar = nodes.iter().find(|n| n.id == "a.rs#Foo.bar").unwrap();
        let kinds: Vec<&str> = bar
            .children
            .iter()
            .filter_map(|c| match &c.kind {
                NodeKind::Syntax(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&"body") && kinds.contains(&"returnType"), "sub-nodes: {kinds:?}");
    }

    #[test]
    fn doc_comment_becomes_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "/// Adds two ints.\n/// Second line.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
        let p = RustProvider::new(root);
        let nodes = p.structure(Path::new("a.rs")).unwrap();
        let add = nodes.iter().find(|n| n.id == "a.rs#add").unwrap();
        let doc = add.children.iter().find(|c| c.id == "a.rs#add:doc").expect("doc anchor");
        assert_eq!(doc.range.start_line, 1, "doc starts at the first /// line: {:?}", doc.range);
        assert!(doc.range.end_line >= 2, "doc spans both /// lines: {:?}", doc.range);
    }

    // TS-parity contract for type members (bench body-edit/schema-field gap analysis): struct
    // fields and enum variants are TOP-LEVEL dotted symbols (they feed the index and
    // retrieval's inline one-line pointers), the container carries :body, and a #[derive]
    // between the doc comment and the item must NOT cost the :doc anchor.
    #[test]
    fn struct_fields_enum_variants_and_docs_through_derives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.rs"),
            "/// One indexed doc.\n#[derive(Clone, Debug)]\npub struct Entry {\n    pub name: String,\n    pub score: f32,\n}\n\n/// What kind.\n#[derive(Clone)]\npub enum Kind {\n    Source,\n    Config,\n}\n",
        )
        .unwrap();
        let p = RustProvider::new(root);
        let nodes = p.structure(Path::new("a.rs")).unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        for want in ["a.rs#Entry", "a.rs#Entry.name", "a.rs#Entry.score", "a.rs#Kind", "a.rs#Kind.Source", "a.rs#Kind.Config"] {
            assert!(ids.contains(&want), "missing {want}: {ids:?}");
        }
        let entry = nodes.iter().find(|n| n.id == "a.rs#Entry").unwrap();
        let child_ids: Vec<&str> = entry.children.iter().map(|c| c.id.as_str()).collect();
        assert!(child_ids.contains(&"a.rs#Entry:body"), "struct :body anchor: {child_ids:?}");
        assert!(
            child_ids.contains(&"a.rs#Entry:doc"),
            "doc survives the #[derive] between it and the struct: {child_ids:?}"
        );
        let score = nodes.iter().find(|n| n.id == "a.rs#Entry.score").unwrap();
        assert_eq!(
            (score.range.start_line, score.range.end_line),
            (5, 5),
            "field is a single-line node (inline-pointer eligible): {:?}",
            score.range
        );
    }

    fn tiny_crate() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        dir
    }

    #[test]
    #[ignore]
    fn rust_gates_replace_node() {
        let dir = tiny_crate();
        let root = dir.path();
        fs::write(root.join("src/lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
        let p = RustProvider::new(root);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        // clean replace -> commits
        let ok = p
            .apply_edits(
                &[EditOp::ReplaceNode {
                    node_id: "src/lib.rs#add".into(),
                    code: "pub fn add(a: i32, b: i32) -> i32 {\n    let s = a + b;\n    s\n}".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean replace must commit: {ok:?}");
        assert!(fs::read_to_string(root.join("src/lib.rs")).unwrap().contains("let s = a + b"));

        // type-error replace -> rejected, disk unchanged
        let before = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceNode {
                    node_id: "src/lib.rs#add".into(),
                    code: "pub fn add(a: i32, b: i32) -> i32 {\n    \"nope\"\n}".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "type error must be rejected: {bad:?}");
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), before, "disk untouched on reject");
    }

    // Bench locate-edit false-clean (2026-07-04): `replace_node RRF_K` with a hallucinated
    // `f64` type committed "type-checked clean" while `RRF_K + rank as f32` was E0277 —
    // rust-analyzer's native pull diagnostics don't emit trait/operator errors AT ALL, a
    // coverage hole no syntactic gap-fill can close (unlike round 4's unresolved imports).
    // The gate verdict therefore comes from `cargo check` now; this pins the exact shape.
    #[test]
    #[ignore]
    fn cross_file_operator_type_error_is_rejected() {
        let dir = tiny_crate();
        let root = dir.path();
        fs::write(
            root.join("src/lib.rs"),
            "pub const K: f32 = 60.0;\npub fn blend(rank: usize) -> f32 {\n    K + rank as f32 + 1.0\n}\n",
        )
        .unwrap();
        let p = RustProvider::new(root);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let before = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        let res = p
            .apply_edits(
                &[EditOp::ReplaceNode {
                    node_id: "src/lib.rs#K".into(),
                    code: "pub const K: f64 = 90.0;".into(),
                }],
                &opts,
            )
            .unwrap();
        match res {
            CommitResult::Rejected { feedback, .. } => {
                assert!(
                    feedback.contains("src/lib.rs"),
                    "reject must name the broken consumer site: {feedback}"
                );
            }
            other => panic!("f32->f64 must be REJECTED (E0277/E0308 at the use site): {other:?}"),
        }
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), before, "disk untouched on reject");

        // The requested change with the TYPE KEPT commits fine.
        let ok = p
            .apply_edits(
                &[EditOp::ReplaceNode {
                    node_id: "src/lib.rs#K".into(),
                    code: "pub const K: f32 = 90.0;".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "same-type value change must commit: {ok:?}");
        assert!(fs::read_to_string(root.join("src/lib.rs")).unwrap().contains("K: f32 = 90.0"));
    }

    #[test]
    #[ignore]
    fn rust_move_file() {
        let dir = tiny_crate();
        let root = dir.path();
        fs::write(root.join("src/lib.rs"), "mod foo;\npub fn run() -> i32 {\n    foo::f()\n}\n").unwrap();
        fs::write(root.join("src/foo.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        let p = RustProvider::new(root);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        let res = p
            .apply_edits(&[EditOp::MoveFile { from: "src/foo.rs".into(), to: "src/bar.rs".into() }], &opts)
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "move should commit: {res:?}");
        assert!(root.join("src/bar.rs").exists(), "file moved to new path");
        assert!(!root.join("src/foo.rs").exists(), "old path gone");
        // rust-analyzer's willRename should rewrite the module decl so it still compiles
        let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(lib.contains("mod bar"), "mod decl rewritten to bar: {lib}");
    }

    // Real gate end-to-end: spawns rust-analyzer (rustup component). #[ignore]; run with
    // `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn rust_analyzer_gates_rename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\npub fn run() -> i32 {\n    add(1, 2)\n}\n",
        )
        .unwrap();

        let p = RustProvider::new(root);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let res = p
            .apply_edits(&[EditOp::Rename { node_id: "src/lib.rs#add".into(), new_name: "sum".into() }], &opts)
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "rename should commit: {res:?}");

        let after = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(after.contains("pub fn sum"), "definition renamed: {after}");
        assert!(after.contains("sum(1, 2)"), "call site renamed by rust-analyzer: {after}");
        assert!(!after.contains("add"), "no 'add' should remain: {after}");
    }

    // Requires docker (or another OCI runtime) up AND the rust image:
    //   docker build -f docker/marksman-rust.Dockerfile -t marksman-rust docker/
    // The `cargo check` gate AND the rust-analyzer rename both run in the container, so this passes
    // with NO host cargo/rustc/rust-analyzer. rust-analyzer sends experimental/serverStatus, which
    // marksman already waits on (wait_quiescent) — so no readiness gotcha, unlike jdtls/sourcekit.
    // RUN ALONE (it sets $CI_SANDBOX): `cargo test -p lang-rust oci_rust -- --ignored --test-threads=1`.
    #[test]
    #[ignore]
    fn oci_rust_gate_and_rename_without_host_tools() {
        if ci_core::oci_runtime().is_none() {
            eprintln!("SKIP: no OCI runtime (docker/podman/nerdctl/container) on PATH");
            return;
        }
        std::env::set_var("CI_SANDBOX", "oci");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\npub fn run() -> i32 {\n    add(1, 2)\n}\n",
        )
        .unwrap();
        let p = RustProvider::open(root, false);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let res = p
            .apply_edits(&[EditOp::Rename { node_id: "src/lib.rs#add".into(), new_name: "sum".into() }], &opts)
            .unwrap();
        std::env::remove_var("CI_SANDBOX");
        assert!(matches!(res, CommitResult::Ok { .. }), "rename commits through the CONTAINER gate: {res:?}");
        let after = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(after.contains("pub fn sum"), "definition renamed in the container: {after}");
        assert!(after.contains("sum(1, 2)"), "call site renamed by the container's rust-analyzer: {after}");
        assert!(!after.contains("add"), "no 'add' remains: {after}");
    }

    // Surgical sub-node edits, gated by rust-analyzer. #[ignore]; run with `--ignored`.
    #[test]
    #[ignore]
    fn rust_subnode_edits_gated() {
        let dir = tiny_crate();
        let root = dir.path();
        fs::write(root.join("src/lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
        let p = RustProvider::new(root);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        // set_body: re-draft just the `{ … }` block, signature untouched -> commits.
        let ok = p
            .apply_edits(
                &[EditOp::SetBody {
                    node_id: "src/lib.rs#add".into(),
                    body: "{\n    let s = a + b;\n    s\n}".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean set_body must commit: {ok:?}");
        let after = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(after.contains("let s = a + b"), "body replaced: {after}");
        assert!(after.contains("pub fn add(a: i32, b: i32) -> i32"), "signature intact: {after}");

        // set_body introducing a type error -> rejected, disk unchanged.
        let before = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::SetBody { node_id: "src/lib.rs#add".into(), body: "{\n    \"nope\"\n}".into() }],
                &opts,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "type-error body must be rejected: {bad:?}");
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), before, "disk untouched on reject");

        // Surgical edit of the `:return` anchor (just the type after `->`) that breaks typing
        // -> rejected. Proves the sub-node anchor is addressable AND still gated.
        let bad_ret = p
            .apply_edits(
                &[EditOp::ReplaceNode { node_id: "src/lib.rs#add:return".into(), code: "String".into() }],
                &opts,
            )
            .unwrap();
        assert!(matches!(bad_ret, CommitResult::Rejected { .. }), "i32 body vs String return must reject: {bad_ret:?}");
    }

    // Opt-in compiler-accurate graph: `rust-analyzer scip` captures a `use` edge that `mod`-only
    // misses (parser → lexer). #[ignore] (spawns rust-analyzer); `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn scip_graph_has_use_edges_mod_misses() {
        let dir = tiny_crate();
        let root = dir.path();
        fs::write(root.join("src/lib.rs"), "mod lexer;\nmod parser;\n").unwrap();
        fs::write(root.join("src/lexer.rs"), "pub struct Token;\n").unwrap();
        fs::write(root.join("src/parser.rs"), "use crate::lexer::Token;\npub fn parse(_t: Token) {}\n").unwrap();

        // The syntactic fallback graph carries `use` edges too (mod + resolved use — the
        // same per-file machinery as the drift overlay): a moved module's consumers must be
        // in the blast radius even with no scip cache (bench move-rust round 4).
        let p = RustProvider::new(root);
        let syn_g = p.import_graph().unwrap();
        assert!(
            syn_g.get(&PathBuf::from("src/parser.rs")).map(|e| e.contains(&PathBuf::from("src/lexer.rs"))).unwrap_or(false),
            "syntactic graph must carry the use edge parser->lexer"
        );

        // SCIP graph: the `use crate::lexer::Token` dependency IS captured.
        refresh_scip(root).expect("rust-analyzer scip");
        let cache = root.join(".marksman/rust.scip");
        assert!(cache.is_file(), "scip cache written");
        let scip_g = ci_scip::ScipIndex::load(&cache).unwrap().import_graph().unwrap();
        let edges = scip_g.get(&PathBuf::from("src/parser.rs")).expect("parser.rs edges from scip");
        assert!(edges.contains(&PathBuf::from("src/lexer.rs")), "scip use edge parser->lexer: {edges:?}");
    }

    // Regression (poisoned engine buffers): a DRY-RUN gate pushes renamed overlay content into
    // rust-analyzer; without the pre-edit `sync_disk`, the subsequent REAL rename sees the
    // caller file already renamed in-buffer, finds no references, and silently renames only the
    // definition (spans against phantom state; caught by T7's first bench run). #[ignore].
    #[test]
    #[ignore]
    fn rename_after_dry_run_hits_all_references() {
        let dir = tiny_crate();
        let root = dir.path();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\npub fn run() -> i32 {\n    add(1, 2)\n}\n",
        )
        .unwrap();
        let p = RustProvider::new(root);
        let op = [EditOp::Rename { node_id: "src/lib.rs#add".into(), new_name: "sum".into() }];

        let dry = p.apply_edits(&op, &EditOpts { write: false, dry_run: true, tsconfig: None }).unwrap();
        assert!(matches!(dry, CommitResult::Ok { .. }), "dry run gates clean: {dry:?}");
        assert!(fs::read_to_string(root.join("src/lib.rs")).unwrap().contains("pub fn add"), "dry run wrote nothing");

        let real = p.apply_edits(&op, &EditOpts { write: true, dry_run: false, tsconfig: None }).unwrap();
        assert!(matches!(real, CommitResult::Ok { .. }), "real run commits: {real:?}");
        let after = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(after.contains("pub fn sum"), "definition renamed: {after}");
        assert!(after.contains("sum(1, 2)"), "CALL SITE renamed too — buffers were synced to disk: {after}");
        assert!(!after.contains("add"), "no stale 'add' anywhere: {after}");
    }

    // Freshness parity with lang-ts: the scip-backed graph must (a) pick up a committed
    // edit's new `use` edge IN-SESSION without re-running rust-analyzer, (b) let a fresh
    // provider (next session) see the same edge via the fingerprint-driven overlay, and
    // (c) refuse a fingerprint-less cache outright instead of serving a graph of unknown
    // age. #[ignore] (runs `rust-analyzer scip`); `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn scip_graph_stays_fresh_after_edits() {
        let dir = tiny_crate();
        let root = dir.path();
        fs::write(root.join("src/lib.rs"), "pub mod lexer;\npub mod parser;\n").unwrap();
        fs::write(root.join("src/lexer.rs"), "pub struct Token;\n").unwrap();
        fs::write(root.join("src/parser.rs"), "pub fn parse() {}\n").unwrap();

        refresh_scip(root).expect("rust-analyzer scip");
        let parser = PathBuf::from("src/parser.rs");
        let lexer = PathBuf::from("src/lexer.rs");
        let has_edge = |g: &ImportGraph| g.get(&parser).map(|e| e.contains(&lexer)).unwrap_or(false);

        let p = RustProvider::new(root).with_scip(true);
        assert!(!has_edge(&p.import_graph().unwrap()), "no parser->lexer edge before the edit");

        // Committed edit introduces `use crate::lexer::Token` — the edge appears in-session.
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        let res = p
            .apply_edits(
                &[EditOp::ReplaceNode {
                    node_id: "src/parser.rs#parse".into(),
                    code: "use crate::lexer::Token;\npub fn parse(_t: Token) {}".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "edit must commit: {res:?}");
        assert!(has_edge(&p.import_graph().unwrap()), "new use edge visible in-session, no reindex");

        // A NEW provider (next session) sees it too: the fingerprint pinpoints the drifted
        // file and its edges come from tree-sitter while the rest stay scip.
        let p2 = RustProvider::new(root).with_scip(true);
        assert!(has_edge(&p2.import_graph().unwrap()), "drift overlay serves the edge across sessions");

        // No fingerprint -> the SCIP cache is refused; the syntactic fallback (mod + use,
        // which now also carries this edge) serves instead — never a cache of unknown age.
        // Distinguish the sources by scope: delete the USE line on disk; a refused cache
        // means the syntactic graph re-reads disk and the edge disappears, while a (wrongly)
        // trusted cache would still serve the stale edge.
        fs::remove_file(root.join(".marksman/rust.scip.fingerprint.json")).unwrap();
        fs::write(root.join("src/parser.rs"), "pub fn parse() {}\n").unwrap();
        let p3 = RustProvider::new(root).with_scip(true);
        assert!(!has_edge(&p3.import_graph().unwrap()), "fingerprint-less cache must not be trusted (edge must reflect DISK, not the stale cache)");
    }

    #[test]
    fn import_graph_follows_mod_decls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/sub")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod foo;\nmod sub;\n").unwrap();
        fs::write(root.join("src/foo.rs"), "pub fn f() {}\n").unwrap();
        fs::write(root.join("src/sub.rs"), "// dir module\n").unwrap(); // sub.rs OR sub/mod.rs
        let p = RustProvider::new(root);
        let g = p.import_graph().unwrap();
        let edges = g.get(&PathBuf::from("src/lib.rs")).expect("lib.rs edges");
        assert!(edges.contains(&PathBuf::from("src/foo.rs")), "mod foo -> foo.rs: {edges:?}");
    }

    // The bench `move-rust` shape end-to-end: ONE move op must complete the whole module
    // move — decl repurposed in lib.rs, parent mod file created, use paths rewritten —
    // via the movefix fallback (ra's willRenameFiles returns nothing here), committing
    // clean with zero manual follow-up. #[ignore]; `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn move_into_submodule_commits_complete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"mv2\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod store;\npub mod tokenize;\n").unwrap();
        fs::write(root.join("src/tokenize.rs"), "pub fn normalize(t: &str) -> String {\n    t.to_lowercase()\n}\n").unwrap();
        fs::write(root.join("src/store.rs"), "use crate::tokenize::normalize;\n\npub fn add(t: &str) -> String {\n    normalize(t)\n}\n").unwrap();

        let p = RustProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::MoveFile { from: "src/tokenize.rs".into(), to: "src/text/tokenize.rs".into() }],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "one move op completes the whole move: {res:?}");
        assert!(root.join("src/text/tokenize.rs").is_file() && !root.join("src/tokenize.rs").exists());
        let modrs = fs::read_to_string(root.join("src/text/mod.rs")).expect("parent module file created");
        assert!(modrs.contains("pub mod tokenize;"), "child declared: {modrs}");
        let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(lib.contains("mod text") && !lib.contains("mod tokenize;"), "decl repurposed: {lib}");
        assert!(fs::read_to_string(root.join("src/store.rs")).unwrap().contains("crate::text::tokenize::normalize"), "use path rewritten");
        let out = std::process::Command::new("cargo").args(["check", "-q"]).current_dir(root).output().unwrap();
        assert!(out.status.success(), "must compile:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    // The bench attempt-2 shape: agents pair move_file with a redundant helper create_file
    // for the parent module file — which the move's fallback ALREADY created in the same
    // batch. That pair must COMMIT (idempotent same-content create), not reject over our own
    // automation. #[ignore]; `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn move_with_redundant_helper_create_still_commits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"mv3\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod store;\npub mod tokenize;\n").unwrap();
        fs::write(root.join("src/tokenize.rs"), "pub fn normalize(t: &str) -> String {\n    t.to_lowercase()\n}\n").unwrap();
        fs::write(root.join("src/store.rs"), "use crate::tokenize::normalize;\n\npub fn add(t: &str) -> String {\n    normalize(t)\n}\n").unwrap();

        let p = RustProvider::new(root);
        let res = p
            .apply_edits(
                &[
                    EditOp::MoveFile { from: "src/tokenize.rs".into(), to: "src/text/tokenize.rs".into() },
                    // trailing space, no newline — must still count as the same intent
                    EditOp::CreateFile { path: "src/text/mod.rs".into(), code: "pub mod tokenize; ".into() },
                ],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "redundant helper must not sink the batch: {res:?}");
        let out = std::process::Command::new("cargo").args(["check", "-q"]).current_dir(root).output().unwrap();
        assert!(out.status.success(), "must compile:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    // Bench move-rust round 4, attempt 1: the agent's COMPLETE plan — move + helper create +
    // helper text edits that movefix's own rewrite also performs. Same-intent redundancy must
    // be SATISFIED (idempotent create, satisfied replace), committing the whole batch in ONE
    // call. #[ignore]; `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn move_batch_with_redundant_text_helpers_commits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"mv4\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod store;\npub mod tokenize;\n").unwrap();
        fs::write(root.join("src/tokenize.rs"), "pub fn normalize(t: &str) -> String {\n    t.to_lowercase()\n}\n").unwrap();
        fs::write(root.join("src/store.rs"), "use crate::tokenize::normalize;\n\npub fn add(t: &str) -> String {\n    normalize(t)\n}\n").unwrap();

        let p = RustProvider::new(root);
        let res = p
            .apply_edits(
                &[
                    EditOp::MoveFile { from: "src/tokenize.rs".into(), to: "src/text/tokenize.rs".into() },
                    EditOp::CreateFile { path: "src/text/mod.rs".into(), code: "pub mod tokenize;\n".into() },
                    EditOp::ReplaceInFile { path: "src/lib.rs".into(), old_text: "pub mod tokenize;".into(), new_text: "pub mod text;".into() },
                    EditOp::ReplaceInFile { path: "src/store.rs".into(), old_text: "use crate::tokenize::normalize;".into(), new_text: "use crate::text::tokenize::normalize;".into() },
                ],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "the complete redundant batch commits in ONE call: {res:?}");
        let out = std::process::Command::new("cargo").args(["check", "-q"]).current_dir(root).output().unwrap();
        assert!(out.status.success(), "must compile:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    // Bench move-rust round 4, attempt 3: the repo was ALREADY broken pre-batch (the mod decl
    // hand-edited, use paths not), so the baseline diff legally excuses those errors — the
    // commit is fine per clause 5. What was WRONG was the result claiming a clean radius.
    // The commit must CARRY the excused breakage (preexisting_in_radius naming store.rs) so
    // the response can hand the agent the remaining fixes instead of "COMPLETE, don't verify".
    // A reject naming store.rs is also acceptable (engine-dependent timing).
    // #[ignore]; `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn move_after_manual_decl_edit_must_not_false_clean() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"mv5\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        // The decl already points at `text` (agent hand-edited it); the use path does NOT.
        fs::write(root.join("src/lib.rs"), "pub mod store;\npub mod text;\n").unwrap();
        fs::write(root.join("src/tokenize.rs"), "pub fn normalize(t: &str) -> String {\n    t.to_lowercase()\n}\n").unwrap();
        fs::write(root.join("src/store.rs"), "use crate::tokenize::normalize;\n\npub fn add(t: &str) -> String {\n    normalize(t)\n}\n").unwrap();

        let p = RustProvider::new(root);
        let res = p
            .apply_edits(
                &[
                    EditOp::MoveFile { from: "src/tokenize.rs".into(), to: "src/text/tokenize.rs".into() },
                    EditOp::CreateFile { path: "src/text/mod.rs".into(), code: "pub mod tokenize;\n".into() },
                ],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        match res {
            CommitResult::Rejected { feedback, .. } => {
                assert!(feedback.contains("store.rs"), "reject must name the broken importer:\n{feedback}");
                assert!(root.join("src/tokenize.rs").is_file(), "reject leaves disk untouched");
            }
            CommitResult::Ok { preexisting_in_radius, .. } => {
                assert!(
                    preexisting_in_radius.iter().any(|d| d.file.contains("store.rs")),
                    "a commit over a pre-broken radius must CARRY the excused breakage naming store.rs, got: {preexisting_in_radius:?}"
                );
            }
        }
    }

    // The R2-bench false-clean, as an invariant: a COMMITTED move must leave a compiling
    // crate. The gate has to SEE the staged deletion — rust-analyzer resolves module paths
    // against the file system, so a move whose source is still on disk (and still open as a
    // buffer) can gate "clean" while `pub mod …;` now points at nothing. We don't pin what
    // rust-analyzer's willRenameFiles rewrites; whichever way it goes, commit ⇒ `cargo check`
    // passes, reject ⇒ disk untouched. #[ignore]; `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn committed_move_must_leave_a_compiling_crate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"mv\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod util;\n\npub fn go() -> i32 {\n    util::one()\n}\n").unwrap();
        fs::write(root.join("src/util.rs"), "pub fn one() -> i32 {\n    1\n}\n").unwrap();

        let p = RustProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::MoveFile { from: "src/util.rs".into(), to: "src/core/util.rs".into() }],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        match res {
            CommitResult::Ok { .. } => {
                let out = std::process::Command::new("cargo")
                    .args(["check", "-q"])
                    .current_dir(root)
                    .output()
                    .expect("cargo check runs");
                assert!(
                    out.status.success(),
                    "gate committed a move that does NOT compile (false clean):\n{}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            CommitResult::Rejected { .. } => {
                assert!(root.join("src/util.rs").is_file(), "reject must leave disk untouched");
                assert!(!root.join("src/core/util.rs").exists(), "reject must not leave the destination behind");
            }
        }
    }

    // A BARE create_file of an undeclared module must commit with the synthesized
    // `pub mod x;` present in the parent decl file — cargo would happily compile the orphan
    // (it just never builds it), so the declaration IS the correctness claim here; the
    // cargo check proves the synthesized decl resolves. #[ignore]; `cargo test -p lang-rust
    // -- --ignored`.
    #[test]
    #[ignore]
    fn bare_create_of_undeclared_module_commits_and_compiles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"cr\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod store;\n").unwrap();
        fs::write(root.join("src/store.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();

        let p = RustProvider::new(root);
        let res = p
            .apply_edits(
                &[EditOp::CreateFile {
                    path: "src/util.rs".into(),
                    code: "pub fn one() -> i32 {\n    1\n}\n".into(),
                }],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "bare create of an undeclared module must commit: {res:?}");
        let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(lib.contains("pub mod util;"), "declaration synthesized in the parent: {lib}");
        assert!(fs::read_to_string(root.join("src/util.rs")).unwrap().contains("pub fn one"));
        let out = std::process::Command::new("cargo").args(["check", "-q"]).current_dir(root).output().unwrap();
        assert!(out.status.success(), "must compile:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    // add_symbol end to end behind the cargo gate: a new #[test] fn appended to an existing
    // module commits (with server-side spacing) and compiles; a type-broken append rejects
    // atomically. #[ignore]; `cargo test -p lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn add_symbol_commits_behind_the_gate() {
        let dir = tiny_crate();
        let root = dir.path();
        fs::write(root.join("src/lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
        let p = RustProvider::new(root);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        let res = p
            .apply_edits(
                &[EditOp::AddSymbol {
                    path: "src/lib.rs".into(),
                    code: "#[test]\nfn add_works() {\n    assert_eq!(add(2, 2), 4);\n}".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "clean append must commit: {res:?}");
        let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert!(
            lib.ends_with("}\n\n#[test]\nfn add_works() {\n    assert_eq!(add(2, 2), 4);\n}\n"),
            "appended at EOF with one blank line + trailing newline: {lib:?}"
        );
        let out = std::process::Command::new("cargo").args(["check", "-q"]).current_dir(root).output().unwrap();
        assert!(out.status.success(), "must compile:\n{}", String::from_utf8_lossy(&out.stderr));

        let before = fs::read_to_string(root.join("src/lib.rs")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::AddSymbol {
                    path: "src/lib.rs".into(),
                    code: "pub fn broken() -> i32 {\n    \"nope\"\n}".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "type-broken append must reject: {bad:?}");
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), before, "disk untouched on reject");
    }

    // The delete refusal's OWN recipe, end to end: its `fix` lines must apply VERBATIM, and
    // the re-issued batch (fixes + delete_file LAST) must commit clean — the refusal text
    // promises exactly this flow, so it is contract, not prose. #[ignore]; `cargo test -p
    // lang-rust -- --ignored`.
    #[test]
    #[ignore]
    fn delete_refusal_fixes_reissued_batch_commits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"del\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod gone;\npub mod user;\n").unwrap();
        fs::write(root.join("src/gone.rs"), "pub fn g() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join("src/user.rs"), "pub use crate::gone::g;\n\npub fn u() -> i32 {\n    2\n}\n").unwrap();

        let p = RustProvider::new(root);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        let res = p.apply_edits(&[EditOp::DeleteFile { path: "src/gone.rs".into() }], &opts).unwrap();
        let feedback = match res {
            CommitResult::Rejected { feedback, .. } => feedback,
            other => panic!("delete of a still-imported file must be refused: {other:?}"),
        };
        assert!(root.join("src/gone.rs").is_file(), "refusal leaves disk untouched");

        // Apply each `fix` VERBATIM — parsed straight out of the refusal text, nothing edited.
        let mut ops: Vec<EditOp> = Vec::new();
        for line in feedback.lines() {
            if let Some(json) = line.trim().strip_prefix("fix (ready to copy): ") {
                let v: serde_json::Value = serde_json::from_str(json).expect("fix must be valid JSON");
                assert_eq!(v["action"], "replace_text", "fix action shape: {v}");
                ops.push(EditOp::ReplaceInFile {
                    path: v["path"].as_str().unwrap().into(),
                    old_text: v["oldText"].as_str().unwrap().into(),
                    new_text: v["newText"].as_str().unwrap().into(),
                });
            }
        }
        assert!(ops.len() >= 2, "one fix per referencing line (mod decl + re-export), got {} in:\n{feedback}", ops.len());
        ops.push(EditOp::DeleteFile { path: "src/gone.rs".into() });

        let res = p.apply_edits(&ops, &opts).unwrap();
        assert!(matches!(res, CommitResult::Ok { .. }), "the refusal's own recipe must commit: {res:?}");
        assert!(!root.join("src/gone.rs").exists(), "file deleted");
        let out = std::process::Command::new("cargo").args(["check", "-q"]).current_dir(root).output().unwrap();
        assert!(out.status.success(), "must compile:\n{}", String::from_utf8_lossy(&out.stderr));
    }
}
