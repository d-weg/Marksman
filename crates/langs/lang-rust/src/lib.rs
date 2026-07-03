//! lang-rust — the Rust [`LanguageProvider`]. v0 read path: in-process `tree-sitter-rust`
//! (no external tooling — Rust's parser is a Rust crate) for `structure()` (items + fn
//! sub-nodes) and `import_graph()` (`mod` resolution). Compiler-accurate references and
//! type-checked edits via rust-analyzer are on the roadmap; this is what lets Marksman
//! index and retrieve Rust — including its own source — today.
use ci_core::{
    CommitResult, EditOp, EditOpts, Error, Granularity, ImportGraph, LanguageProvider, Node,
    NodeKind, Range, Result, SymbolKind,
};
use ci_edit::GateEngine;
use ci_lsp::LspClient;
use ci_treesitter::{syntax_node, ts_range};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tree_sitter::{Node as TsNode, Parser};

mod movefix;
mod usegraph;

/// The rust-analyzer LSP server, loaded once and reused as the edit/gate engine — the same
/// `GateEngine`/`LspClient` path TypeScript uses, just rust-analyzer instead of tsserver.
type WarmEngine = Arc<Mutex<Option<RustEngine>>>;

/// The Rust write engine: rust-analyzer for diagnostics/rename, plus a SYNTACTIC module-move
/// fallback for the one operation ra's `willRenameFiles` doesn't cover (moves into a
/// submodule return NO edits, leaving the `mod` decl and every `crate::` path dangling —
/// bench `move-rust`). The fallback emits a genuine WorkspaceEdit (see `movefix`); the gate
/// still verifies the result, so an unsupported shape degrades to a REJECT with named sites,
/// never a silent break.
struct RustEngine {
    root: PathBuf,
    lsp: LspClient,
}

/// Diagnostics for references to files the CURRENT BATCH deletes (empty-content buffers, the
/// gate's deletion convention): `use crate::a::b…` chains and `mod x;` decls resolving to a
/// deleted path. This is the E0432/E0583 class rust-analyzer's pull diagnostics never report.
fn deleted_path_references(root: &Path, files: &[(String, String)]) -> Vec<ci_core::Diag> {
    let deleted: std::collections::HashSet<&str> =
        files.iter().filter(|(_, c)| c.is_empty()).map(|(f, _)| f.as_str()).collect();
    if deleted.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (rel, content) in files.iter().filter(|(_, c)| !c.is_empty()) {
        for (i, line) in content.lines().enumerate() {
            // `crate::a::b::…` — walk the segment chain; any prefix landing on a deleted
            // module file is a stranded reference.
            let mut rest = line;
            while let Some(pos) = rest.find("crate::") {
                let tail = &rest[pos + 7..];
                let segs: Vec<&str> = tail
                    .split("::")
                    .map(|s| s.trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '_')))
                    .take_while(|s| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_'))
                    .collect();
                for n in 1..=segs.len() {
                    let base = segs[..n].join("/");
                    for cand in [format!("src/{base}.rs"), format!("src/{base}/mod.rs")] {
                        if deleted.contains(cand.as_str()) {
                            out.push(ci_core::Diag {
                                file: rel.clone(),
                                code: 0,
                                message: format!(
                                    "unresolved import `crate::{}` — {cand} is deleted/moved by this batch (E0432); update the path",
                                    segs[..n].join("::")
                                ),
                                line: i as u32 + 1,
                            });
                        }
                    }
                }
                rest = &rest[pos + 7..];
            }
            // `mod x;` decls whose file this batch deletes (E0583-class, decl side).
            let t = line.trim_start();
            let decl = t.strip_prefix("pub ").unwrap_or(t);
            if let Some(m) = decl.strip_prefix("mod ") {
                if let Some(name) = m.trim_end().strip_suffix(';') {
                    if let Some(target) = resolve_mod(root, rel, name.trim()) {
                        let target = target.to_string_lossy().replace('\\', "/");
                        if deleted.contains(target.as_str()) {
                            out.push(ci_core::Diag {
                                file: rel.clone(),
                                code: 0,
                                message: format!(
                                    "`mod {}` points at {target}, which this batch deletes/moves (E0583); update or remove the declaration",
                                    name.trim()
                                ),
                                line: i as u32 + 1,
                            });
                        }
                    }
                }
            }
        }
    }
    out
}

fn workspace_edit_is_empty(we: &serde_json::Value) -> bool {
    use serde_json::Value;
    let dc = we.get("documentChanges").and_then(Value::as_array).map(|a| a.is_empty()).unwrap_or(true);
    let ch = we
        .get("changes")
        .and_then(Value::as_object)
        .map(|o| o.values().all(|v| v.as_array().map(|a| a.is_empty()).unwrap_or(true)))
        .unwrap_or(true);
    dc && ch
}

impl GateEngine for RustEngine {
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<ci_core::Diag>> {
        let mut out = self.lsp.diagnostics(files)?;
        // Gap-fill a rust-analyzer blind spot: its native (pull) diagnostics DO NOT include
        // unresolved imports — `use crate::gone::x;` returns ZERO diagnostics even steady-
        // state (verified directly; rustc-grade errors live in its cargo-check integration,
        // which is far too slow per edit). So every move/delete that stranded a consumer
        // gated "clean" (bench move-rust, three rounds of it). The gate marks batch-deleted
        // files as EMPTY buffers; flag any `crate::…` path or `mod x;` decl that resolves to
        // one of them — deterministic, buffer-aware, zero false positives on live code.
        out.extend(deleted_path_references(&self.root, files));
        Ok(out)
    }
    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<serde_json::Value> {
        GateEngine::rename(&mut self.lsp, file, line, character, new_name)
    }
    fn will_rename(&mut self, from: &str, to: &str) -> Result<serde_json::Value> {
        let we = GateEngine::will_rename(&mut self.lsp, from, to)?;
        if workspace_edit_is_empty(&we) {
            if let Some(fix) = movefix::move_workspace_edit(&self.root, from, to) {
                return Ok(fix);
            }
        }
        Ok(we)
    }
    fn sync_disk(&mut self) -> Result<()> {
        self.lsp.sync_disk()
    }
    fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        self.lsp.fs_events(created, deleted)
    }
}

#[derive(Clone)]
pub struct RustProvider {
    root: PathBuf,
    engine: WarmEngine,
    /// Use the cached `rust-analyzer scip` graph (compiler-accurate `use` edges) over the
    /// tree-sitter `mod` graph. Set by the caller from `Config::scip_enabled("rust")`.
    use_scip: bool,
    /// The scip cache's base graph, loaded + drift-checked ONCE per provider (`None` inside =
    /// cache unusable: absent, unreadable, or no fingerprint to trust it by).
    scip_base: Arc<Mutex<Option<Option<ImportGraph>>>>,
    /// Per-file outgoing-edge overrides on top of the scip base graph (`None` = file deleted).
    /// Seeded at load for files that drifted since the cache was built, updated after each
    /// committed edit — so the served graph never reports pre-edit edges. Same pattern as
    /// lang-ts's post-edit `fresh` overlay, sourced from tree-sitter (`mod` + resolved `use`).
    fresh_edges: Arc<Mutex<HashMap<String, Option<Vec<PathBuf>>>>>,
}

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
        Self {
            root: root.to_path_buf(),
            engine: Arc::new(Mutex::new(None)),
            use_scip: false,
            scip_base: Arc::new(Mutex::new(None)),
            fresh_edges: Arc::new(Mutex::new(HashMap::new())),
        }
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

    /// Enable the compiler-accurate `rust-analyzer scip` graph (see [`RustProvider::use_scip`]).
    pub fn with_scip(mut self, use_scip: bool) -> Self {
        self.use_scip = use_scip;
        self
    }

    /// Normalize a (possibly absolute) path to the repo-relative posix form.
    fn rel(&self, file: &Path) -> String {
        let p = if file.is_absolute() { file.strip_prefix(&self.root).unwrap_or(file) } else { file };
        p.to_string_lossy().replace('\\', "/")
    }

    pub(crate) fn parse(content: &str) -> Option<tree_sitter::Tree> {
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).ok()?;
        parser.parse(content, None)
    }

    /// The cached `rust-analyzer scip` index (the optional compiler-accurate graph source).
    fn scip_cache(&self) -> PathBuf {
        self.root.join(".marksman").join("rust.scip")
    }

    /// The `use`/reference import graph from the cached SCIP index, kept honest: the base
    /// graph is loaded once and trusted only as far as its fingerprint reaches — every file
    /// that drifted since `refresh_scip` ran gets its edges recomputed from tree-sitter
    /// (`mod` + resolved `use` paths) as an overlay, and committed edits update that overlay
    /// (see `apply_edits`). `None` (→ the `mod`-graph fallback) when there is no cache, or a
    /// cache with no fingerprint at all — a graph we can't vouch for is never served.
    fn scip_graph(&self) -> Option<ImportGraph> {
        let mut base_slot = self.scip_base.lock().ok()?;
        if base_slot.is_none() {
            *base_slot = Some(self.load_scip_base());
        }
        let base = base_slot.as_ref()?.clone()?;
        drop(base_slot);
        let overrides = self.fresh_edges.lock().ok()?;
        Some(usegraph::overlay_graph(base, &overrides))
    }

    /// Load + drift-check the scip cache (once per provider). On success, seeds `fresh_edges`
    /// for exactly the files that changed since the cache was built.
    fn load_scip_base(&self) -> Option<ImportGraph> {
        let cache = self.scip_cache();
        if !cache.is_file() {
            return None;
        }
        let graph = ci_scip::ScipIndex::load(&cache).ok()?.import_graph().ok()?;
        let Some(drift) = usegraph::drifted_files(&self.root) else {
            eprintln!(
                "[lang-rust] scip cache {} has no fingerprint — refusing to serve a graph of unknown \
                 age (falling back to the mod graph); re-run `index` to regenerate it",
                cache.display()
            );
            return None;
        };
        if !drift.is_empty() {
            if let Ok(mut m) = self.fresh_edges.lock() {
                for rel in &drift {
                    if rel.ends_with(".rs") {
                        m.entry(rel.clone()).or_insert_with(|| self.edges_from_disk(rel));
                    }
                }
            }
            eprintln!(
                "[lang-rust] scip graph: {} file(s) changed since the cache was built; serving \
                 tree-sitter edges for those files (scip edges for the rest)",
                drift.len()
            );
        }
        Some(graph)
    }

    /// The instant in-process graph: tree-sitter `mod` + resolved `use` edges over the whole
    /// repo — the SAME per-file machinery the scip drift overlay uses. It must include `use`
    /// edges: a file's importers via `use crate::x::…` are exactly the blast radius a
    /// move/delete needs, and the `mod`-only graph left them out (a moved module's consumers
    /// were invisible to the gate when no scip cache existed — bench move-rust round 4).
    fn syntactic_graph(&self) -> ImportGraph {
        let mut graph: ImportGraph = BTreeMap::new();
        for rel in rust_files(&self.root) {
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
        let tree = Self::parse(&content)?;
        Some(usegraph::file_edges(&self.root, rel, tree.root_node(), content.as_bytes()))
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
        if let Some(drift) = usegraph::drifted_files(root) {
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
    usegraph::store_fingerprint(root)
        .map_err(|e| Error::Driver(format!("storing the scip cache fingerprint: {e}")))?;
    Ok(())
}

impl LanguageProvider for RustProvider {
    fn granularity(&self) -> Granularity {
        Granularity::Ast // tree-sitter sub-nodes (params / return / body)
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        let rel = self.rel(file);
        let content = match std::fs::read_to_string(self.root.join(&rel)) {
            Ok(c) => c,
            Err(_) => return Ok(vec![]),
        };
        let Some(tree) = Self::parse(&content) else { return Ok(vec![]) };
        let bytes = content.as_bytes();
        let mut out = Vec::new();
        let prefix = format!("{rel}#");
        collect_items(tree.root_node(), bytes, &prefix, SymbolKind::Function, &mut out);
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

    /// Start rust-analyzer and load the cargo workspace NOW, on a background thread, so the
    /// first `apply_edits` finds it warm instead of paying the cold `cargo metadata` + analysis
    /// inline. No-op-safe if rust-analyzer can't start (apply_edits then surfaces the error).
    fn prewarm(&self) {
        let slot = self.engine.clone();
        let root = self.root.clone();
        let warm = rust_files(&root)
            .into_iter()
            .find_map(|rel| std::fs::read_to_string(root.join(&rel)).ok().map(|c| (rel, c)));
        std::thread::spawn(move || {
            let Ok(mut guard) = slot.lock() else { return };
            if guard.is_some() {
                return;
            }
            if let Ok(mut client) = LspClient::start(&root, rust_analyzer_command()) {
                if let Some((f, content)) = warm {
                    let _ = client.diagnostics(&[(f, content)]); // forces the workspace to load
                }
                *guard = Some(RustEngine { root: root.clone(), lsp: client });
            }
        });
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        // Gate via the PERSISTENT rust-analyzer LSP (reuse from prewarm; lock blocks until an
        // in-flight warm finishes, so we never start a second cold server). Same VFS +
        // baseline-diff + blast-radius path as TypeScript, through the GateEngine seam.
        let mut guard = self.engine.lock().map_err(|_| Error::Driver("engine lock poisoned".into()))?;
        if guard.is_none() {
            let lsp = LspClient::start(&self.root, rust_analyzer_command()).map_err(|e| {
                // When the toolchain itself is the problem, say THAT (with the install hint)
                // instead of a raw spawn error — reads worked fine, so this is the user's first
                // signal that the WRITE path has a missing dependency.
                match toolchain().describe_missing() {
                    Some(missing) => Error::Driver(format!("rust edit engine failed to start ({e}).\n{missing}")),
                    None => e,
                }
            })?;
            *guard = Some(RustEngine { root: self.root.clone(), lsp });
        }
        let engine: &mut dyn GateEngine = guard.as_mut().unwrap();

        let structure_of = |f: &str| self.structure(Path::new(f)).unwrap_or_default();

        // Blast-radius policy follows the graph's edge semantics (the ReadIndex rule, bench
        // T9): the compiler-accurate scip `use` graph flattens re-exports, so ONE reverse hop
        // is sound; the syntactic fallback does not, so its radius must expand TRANSITIVELY
        // or a `pub use` chain hides consumers from the gate.
        let (graph, semantic) = match self.use_scip.then(|| self.scip_graph()).flatten() {
            Some(g) => (g, true),
            None => (self.syntactic_graph(), false),
        };
        let reverse = ci_core::reverse_import_map(&graph);
        let reverse_imports = |file: &str| {
            if semantic {
                reverse.get(file).cloned().unwrap_or_default()
            } else {
                ci_core::transitive_reverse_imports(&reverse, file)
            }
        };

        let r = ci_edit::commit_edits(&self.root, ops, &structure_of, engine, opts, &reverse_imports);
        // Keep the scip-backed graph true in-session: re-describe each committed file's edges
        // from its new content (tree-sitter, in-process — cheap and can't fail the edit). The
        // live mod graph and structure() read disk directly, so they need no help.
        if self.use_scip {
            if let Ok(CommitResult::Ok { changed_files, .. }) = &r {
                if opts.write && !opts.dry_run {
                    if let Ok(mut m) = self.fresh_edges.lock() {
                        for f in changed_files {
                            let rel = f.to_string_lossy().replace('\\', "/");
                            if rel.ends_with(".rs") {
                                m.insert(rel.clone(), self.edges_from_disk(&rel));
                            }
                        }
                    }
                }
            }
        }
        r
    }
}

// ── structure ──────────────────────────────────────────────────────────────

/// Walk an item list, emitting a `Node` per named declaration. `fn_kind` is the kind for
/// `function_item`s found here (Function at top level, Method inside an `impl`). `prefix` is
/// the id stem (`"file.rs#"`, or `"file.rs#Type."` inside an impl).
fn collect_items(node: TsNode, bytes: &[u8], prefix: &str, fn_kind: SymbolKind, out: &mut Vec<Node>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(mut n) = named_node(&child, bytes, prefix, fn_kind) {
                    add_fn_subnodes(&mut n, &child, bytes);
                    out.push(n);
                }
            }
            "struct_item" | "union_item" => push(&child, bytes, prefix, SymbolKind::Struct, out),
            "enum_item" => push(&child, bytes, prefix, SymbolKind::Enum, out),
            "trait_item" => push(&child, bytes, prefix, SymbolKind::Interface, out),
            "type_item" => push(&child, bytes, prefix, SymbolKind::TypeAlias, out),
            "const_item" | "static_item" => push(&child, bytes, prefix, SymbolKind::Variable, out),
            "macro_definition" => push(&child, bytes, prefix, SymbolKind::Function, out),
            "impl_item" => {
                let ty = child
                    .child_by_field_name("type")
                    .and_then(|t| type_text(&t, bytes))
                    .unwrap_or_else(|| "impl".to_string());
                if let Some(body) = child.child_by_field_name("body") {
                    let inner = format!("{prefix}{ty}.");
                    collect_items(body, bytes, &inner, SymbolKind::Method, out);
                }
            }
            "mod_item" => {
                if let Some(body) = child.child_by_field_name("body") {
                    collect_items(body, bytes, prefix, SymbolKind::Function, out);
                }
            }
            _ => {}
        }
    }
}

fn push(item: &TsNode, bytes: &[u8], prefix: &str, kind: SymbolKind, out: &mut Vec<Node>) {
    if let Some(n) = named_node(item, bytes, prefix, kind) {
        out.push(n);
    }
}

/// Build a declaration `Node` from an item with a `name` field. Attaches a `:doc` sub-node for
/// the item's leading doc comments (`///` / `//!` / `/** */`) so they're editable like any anchor.
fn named_node(item: &TsNode, bytes: &[u8], prefix: &str, kind: SymbolKind) -> Option<Node> {
    let name_node = item.child_by_field_name("name")?;
    let name = name_node.utf8_text(bytes).ok()?.to_string();
    let id = format!("{prefix}{name}");
    let mut children = Vec::new();
    if let Some(r) = doc_range(item) {
        children.push(Node {
            id: format!("{id}:doc"),
            name: None,
            kind: NodeKind::Syntax("doc".to_string()),
            range: r,
            name_range: None,
            children: vec![],
        });
    }
    Some(Node {
        id,
        name: Some(name),
        kind: NodeKind::Symbol(kind),
        range: ts_range(item),
        name_range: Some(ts_range(&name_node)),
        children,
    })
}

/// Range spanning the contiguous leading comment lines directly above `item` (Rust doc comments
/// `///` / `//!` are `line_comment`s; `/** */` is a `block_comment`). v0 stops at a non-comment
/// sibling, so a doc comment separated from the item by an attribute isn't captured yet.
fn doc_range(item: &TsNode) -> Option<Range> {
    ci_treesitter::leading_comment_range(item, |n| {
        matches!(n.kind(), "line_comment" | "block_comment")
    })
}

/// Attach params / return type / body as `Syntax` sub-nodes of a function/method.
fn add_fn_subnodes(n: &mut Node, item: &TsNode, bytes: &[u8]) {
    if let Some(params) = item.child_by_field_name("parameters") {
        // The whole `(...)` list — the insertion anchor for `add_parameter` / a missing return type.
        n.children.push(syntax_node(&format!("{}:params", n.id), None, "params", &params));
        let mut cursor = params.walk();
        for (i, p) in params.named_children(&mut cursor).enumerate() {
            let name = p.utf8_text(bytes).ok().map(str::to_string);
            n.children.push(syntax_node(&format!("{}:param.{i}", n.id), name, "parameter", &p));
        }
    }
    if let Some(rt) = item.child_by_field_name("return_type") {
        n.children.push(syntax_node(&format!("{}:return", n.id), None, "returnType", &rt));
    }
    if let Some(body) = item.child_by_field_name("body") {
        n.children.push(syntax_node(&format!("{}:body", n.id), None, "body", &body));
    }
}

/// First `type_identifier` inside an impl's `type` node (the base type being implemented).
fn type_text(t: &TsNode, bytes: &[u8]) -> Option<String> {
    if t.kind() == "type_identifier" {
        return t.utf8_text(bytes).ok().map(str::to_string);
    }
    let mut cursor = t.walk();
    for c in t.named_children(&mut cursor) {
        if c.kind() == "type_identifier" {
            return c.utf8_text(bytes).ok().map(str::to_string);
        }
    }
    t.utf8_text(bytes).ok().map(str::to_string)
}

// ── import graph (mod resolution) ────────────────────────────────────────────

/// `mod foo;` declarations (file modules only — inline `mod foo { … }` has no file edge).
fn mod_decls(root: TsNode, bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "mod_item" && child.child_by_field_name("body").is_none() {
            if let Some(name) = child.child_by_field_name("name").and_then(|n| n.utf8_text(bytes).ok()) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Resolve `mod <module>;` declared in `from` (repo-relative) to a repo-relative file.
/// A directory module (`mod.rs`, `lib.rs`, `main.rs`) resolves submodules in its own dir;
/// a file module `foo.rs` resolves them under `foo/`.
fn resolve_mod(root: &Path, from: &str, module: &str) -> Option<PathBuf> {
    let from_path = Path::new(from);
    let parent = from_path.parent().unwrap_or(Path::new(""));
    let stem = from_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let base = if matches!(stem, "mod" | "lib" | "main") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    // Resolve `mod module;` to either `<base>/module.rs` or `<base>/module/mod.rs`.
    [base.join(format!("{module}.rs")), base.join(module).join("mod.rs")]
        .into_iter()
        .find(|cand| root.join(cand).is_file())
}

/// Repo-relative `.rs` files, gitignore-aware, skipping `target/`.
fn rust_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !rel.starts_with("target/") {
                out.push(rel);
            }
        }
    }
    out
}

// ── skeletal context ─────────────────────────────────────────────────────────

/// Return `content` with Rust function/method bodies (`block`) elided, keeping signatures.
/// Best-effort: returns the original on a parse failure.
pub fn outline(content: &str) -> String {
    let Some(tree) = RustProvider::parse(content) else { return content.to_string() };
    // Fold each `function_item`'s `block` body; keep everything else (signatures, types).
    let bodies = ci_treesitter::body_ranges(tree.root_node(), &["function_item"], &["block"]);
    ci_core::elide_bodies(content, bodies)
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
