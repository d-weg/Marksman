//! Marksman MCP server (stdio, JSON-RPC 2.0, newline-delimited). Exposes the
//! input tools (retrieve_context, describe_architecture, find_symbols) and the
//! output tools (list_anchors, read_node, apply_edits). Launch per repo:
//!   marksman-mcp --root /path/to/repo   (or $MARKSMAN_ROOT, or cwd)
//!
//! The server is pure-Rust orchestration; all language/external tooling is behind
//! the `lang-ts` provider.
use ci_arch::{build_architecture, format_architecture};
use ci_build::{build_registry, ProviderBuild, ProviderRegistry};
use ci_core::{Config, EditOpts, Manifest, Node, NodeKind};
use ci_edit::{action_to_op, resolve_all_in, resolve_in, Action};
use ci_embed::StaticEmbedder;
use ci_index::{index_dir, index_exists, load_index, save_index, IndexData};
use ci_retrieve::{retrieve, RetrieveOptions};
use ci_proto::ProcessProvider;
use lang_fallback::{FallbackProvider, FbLang};
use lang_rust::RustProvider;
use lang_ts::TsProvider;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Construct the provider for one language, honoring the manifest's vendored binary and
/// `CI_PROVIDER=sidecar`. Called once per active language by [`build_registry`], so a language's
/// toolchain is never probed, fetched, or run unless the repo actually has its files (a
/// Rust-only repo never touches Node). Each language's TOOLCHAIN is checked before any of it
/// runs: a missing dependency becomes `Unavailable` with the install instructions (permanent,
/// carried on the registry), not a cryptic spawn error or a retry loop.
fn make_provider(lang: &str, root: &Path, config: &Config) -> ProviderBuild {
    if std::env::var("CI_PROVIDER").as_deref() == Ok("sidecar") {
        if let Some(cmd) = ci_proto::sidecar_command_with(lang, root, false, config.provider_bin(lang)) {
            eprintln!("[marksman-mcp] language: {lang} (sidecar process — protobuf wire)");
            match ProcessProvider::spawn(cmd) {
                Ok(p) => return ProviderBuild::Ready(Arc::new(p)),
                Err(e) => {
                    eprintln!("[marksman-mcp] sidecar {lang} failed to start ({e}); skipping");
                    return ProviderBuild::Failed(e.to_string());
                }
            }
        }
        eprintln!("[marksman-mcp] CI_PROVIDER=sidecar but no marksman-provider-{lang} found — using in-process");
    }
    match lang {
        "rust" => {
            // Reads are in-process tree-sitter (no external deps) — the provider always comes
            // up. rust-analyzer gates only WRITES: warn now if missing, and apply_edits repeats
            // the same install hint if actually invoked.
            if let Some(missing) = lang_rust::toolchain().describe_missing() {
                eprintln!("[marksman-mcp] warning: {missing}\n  (rust reads work; type-checked edits will fail until installed)");
            }
            eprintln!("[marksman-mcp] language: rust (tree-sitter reads + rust-analyzer scip graph; gate: cargo check, renames: rust-analyzer)");
            ProviderBuild::Ready(Arc::new(RustProvider::open(root, config.scip_enabled("rust"))))
        }
        "ts" => {
            // CI_TS_MODE ablation arms (docs/benchmarks.md): serve TS from tree-sitter instead
            // of SCIP — "treesitter" is the generic UNGATED provider (needs nothing external),
            // "treesitter-gated" keeps the warm ts-morph gate on a tree-sitter read path.
            match std::env::var("CI_TS_MODE").as_deref() {
                Ok("treesitter") => {
                    eprintln!("[marksman-mcp] language: typescript (ABLATION: generic tree-sitter, UNGATED — CI_TS_MODE=treesitter)");
                    return ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, FbLang::Ts)));
                }
                Ok("treesitter-gated") => {
                    if let Some(missing) = lang_ts::toolchain().describe_missing() {
                        eprintln!("[marksman-mcp] typescript DISABLED (gated ablation still needs the gate's toolchain):\n{missing}");
                        return ProviderBuild::Unavailable(missing);
                    }
                    eprintln!("[marksman-mcp] language: typescript (ABLATION: tree-sitter read + ts-morph gate — CI_TS_MODE=treesitter-gated)");
                    return ProviderBuild::Ready(Arc::new(lang_ts::TsTreeGated::new(root)));
                }
                Ok("lsp") => {
                    // COMPARISON arm: index by sweeping the tsgo language server (ci-lsp-index)
                    // instead of scip-typescript; same SCIP read path, different producer.
                    if let Some(missing) = lang_ts::toolchain().describe_missing() {
                        eprintln!("[marksman-mcp] typescript DISABLED (the LSP sweep still needs Node for tsgo via npx):\n{missing}");
                        return ProviderBuild::Unavailable(missing);
                    }
                    eprintln!("[marksman-mcp] language: typescript (COMPARISON: tsgo LSP-sweep index — CI_TS_MODE=lsp)");
                    return match TsProvider::index_with_lsp_sweep(root) {
                        Ok(p) => ProviderBuild::Ready(Arc::new(p)),
                        Err(e) => {
                            eprintln!("[marksman-mcp] tsgo LSP-sweep indexing failed ({e}); skipping TS files");
                            ProviderBuild::Failed(e.to_string())
                        }
                    };
                }
                _ => {}
            }
            // TypeScript needs Node for BOTH paths (scip-typescript index + the gate). Missing
            // toolchain = the language is off, loudly and actionably — never a half-working
            // provider or an ungated fallback.
            if let Some(missing) = lang_ts::toolchain().describe_missing() {
                eprintln!("[marksman-mcp] typescript DISABLED:\n{missing}");
                return ProviderBuild::Unavailable(missing);
            }
            // `open` loads the cached .codeindex/index.scip when the source fingerprint still
            // matches (ms), and re-runs scip-typescript only when it doesn't (~20s).
            eprintln!("[marksman-mcp] language: typescript — opening scip index for {} …", root.display());
            match TsProvider::open(root) {
                Ok(p) => ProviderBuild::Ready(Arc::new(p)),
                Err(e) => {
                    eprintln!("[marksman-mcp] typescript indexing failed ({e}); skipping TS files");
                    ProviderBuild::Failed(e.to_string())
                }
            }
        }
        // Every other supported language rides the generic tree-sitter fallback: full read
        // path, ungated edits, zero external dependencies.
        other => match FbLang::from_name(other) {
            Some(fb) => {
                eprintln!(
                    "[marksman-mcp] language: {} (generic tree-sitter fallback, in-process — edits are ungated)",
                    fb.label()
                );
                ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, fb)))
            }
            None => ProviderBuild::Failed(format!("unknown language '{other}'")),
        },
    }
}

/// The extension → provider registry for `root`, dispatching each file to its language's provider
/// so a mixed repo reads/edits fully. Absent/disabled languages register nothing.
fn build_registry_for(root: &Path) -> Result<ProviderRegistry, String> {
    let mut config = Config::load(root).unwrap_or_default();
    config.index_dir = ".marksman".into();
    let cfg = config.clone();
    let built = build_registry(root, &mut config, |lang| make_provider(lang, root, &cfg)).map_err(|e| e.to_string())?;
    // A language that's present + enabled but whose provider failed to construct (e.g.
    // scip-typescript lost an npx-cache race and exited non-zero) yields an INCOMPLETE registry:
    // its files would silently have no provider, so every read/edit on them degrades to "symbol
    // not found" / "no language provider" and the agent falls back to grep. Such failures are
    // typically transient, so refuse this build rather than let the caller CACHE it for the whole
    // process life — the next tool call rebuilds and retries the toolchain (and a genuinely broken
    // toolchain surfaces loudly on every call instead of being masked by a silent fallback).
    if !built.failed.is_empty() {
        return Err(format!(
            "language provider(s) failed to start: {} — toolchain unavailable or a transient error \
             (e.g. scip-typescript via npx); not caching a degraded registry. Retry the call.",
            built.failed.join(", ")
        ));
    }
    Ok(built.registry)
}

fn resolve_root() -> PathBuf {
    let argv: Vec<String> = std::env::args().collect();
    if let Some(i) = argv.iter().position(|a| a == "--root") {
        if let Some(p) = argv.get(i + 1) {
            return PathBuf::from(p);
        }
    }
    std::env::var("MARKSMAN_ROOT")
        .or_else(|_| std::env::var("CODEINDEX_ROOT")) // legacy name, still honored
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default())
}

fn model_dir() -> PathBuf {
    std::env::var("CI_MODEL_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        // Default to the path the README's download step uses, so the documented
        // `git clone … ~/.marksman/models/potion-code-16M` works without setting CI_MODEL_DIR.
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".marksman/models/potion-code-16M"))
            .unwrap_or_else(|_| PathBuf::from(".marksman/models/potion-code-16M"))
    })
}

struct Server {
    root: PathBuf,
    config: Config,
    // Behind Arc<Mutex> so it can be built + warmed on a background thread at startup
    // (see `start_prewarm`) and cheaply cloned out for each tool call.
    registry: Arc<Mutex<Option<ProviderRegistry>>>,
    embedder: Option<StaticEmbedder>,
    // The loaded retrieval index, keyed on meta.json's mtime (see `index_data`). Mutex (not a
    // plain field) so `&self` methods like files_defining can read through the cache.
    index_cache: Mutex<Option<(std::time::SystemTime, Arc<IndexData>)>>,
}

impl Server {
    fn new(root: PathBuf) -> Self {
        let mut config = Config::load(&root).unwrap_or_default();
        config.embedding_model = "minishlab/potion-code-16M".into();
        config.index_dir = ".marksman".into();
        Server { root, config, registry: Arc::new(Mutex::new(None)), embedder: None, index_cache: Mutex::new(None) }
    }

    /// The retrieval index, cached in memory and keyed on index.pb's mtime. Every tool call
    /// used to re-read + re-parse the whole store and rebuild BM25/graph — pure per-call
    /// waste, linear in repo size. `save_index` rewrites index.pb, so its mtime is the
    /// generation marker: our own reindex_after_edit and an external `marksman index`
    /// both invalidate. The mtime is read BEFORE loading, so a writer racing the load causes
    /// a re-load on the next call rather than a stale cache entry.
    fn index_data(&self) -> Result<Arc<IndexData>, String> {
        let mtime = std::fs::metadata(index_dir(&self.root, &self.config).join("index.pb"))
            .and_then(|m| m.modified())
            .ok();
        let mut cache = self.index_cache.lock().map_err(|_| "index cache lock poisoned".to_string())?;
        if let (Some((cached_at, data)), Some(m)) = (cache.as_ref(), mtime) {
            if *cached_at == m {
                return Ok(data.clone());
            }
        }
        let data = Arc::new(load_index(&self.root, &self.config).map_err(|e| e.to_string())?);
        if let Some(m) = mtime {
            *cache = Some((m, data.clone()));
        }
        Ok(data)
    }

    /// Build the provider registry for the repo AND warm each write engine on a background thread
    /// at startup — so the first output-tool call finds it ready. For a TS repo this runs
    /// scip-typescript + warms the language server; for a Rust repo it's instant (in-process
    /// tree-sitter, no Node). Holding the registry lock across the build means a tool that
    /// needs it mid-build waits, not races.
    fn start_prewarm(&self) {
        let slot = self.registry.clone();
        let root = self.root.clone();
        std::thread::spawn(move || {
            let Ok(mut g) = slot.lock() else { return };
            if g.is_some() {
                return;
            }
            if let Ok(reg) = build_registry_for(&root) {
                reg.prewarm_all();
                *g = Some(reg);
            }
        });
    }

    /// Get the provider registry, building it if `start_prewarm` hasn't finished. Returns a cheap
    /// clone so the caller doesn't hold the lock. Needed by the output tools only.
    fn registry(&self) -> Result<ProviderRegistry, String> {
        let mut g = self.registry.lock().map_err(|_| "registry lock poisoned".to_string())?;
        if g.is_none() {
            let reg = build_registry_for(&self.root)?;
            reg.prewarm_all();
            *g = Some(reg);
        }
        Ok(g.as_ref().unwrap().clone())
    }

    fn embedder(&mut self) -> Result<&StaticEmbedder, String> {
        if self.embedder.is_none() {
            let dir = model_dir();
            ci_embed::ensure_model(&dir, &self.config.embedding_model)?;
            self.embedder = Some(StaticEmbedder::load(&dir).map_err(|e| e.to_string())?);
        }
        Ok(self.embedder.as_ref().unwrap())
    }

    fn retrieve_context(&mut self, args: &Value) -> Result<String, String> {
        let task = args["task"].as_str().ok_or("`task` is required")?.to_string();
        if !index_exists(&self.root, &self.config) {
            return Err("no index — run `marksman index <root>` first".into());
        }
        let index = self.index_data()?;
        let model = self.config.embedding_model.clone();
        let embedder = self.embedder()?;
        ensure_index_matches(&index.meta.model, index.meta.dims, &model, embedder.dim())?;
        let qvec = embedder.embed(&task).map_err(|e| e.to_string())?;
        let opts = RetrieveOptions {
            top_n: args["topN"].as_u64().map(|n| n as usize),
            hops: args["hops"].as_u64().map(|n| n as usize),
            ..Default::default()
        };
        let manifest = retrieve(&self.root, &task, &index, &qvec, &self.config, &opts);
        let detail = args["detailLevel"]
            .as_str()
            .or_else(|| args["detail_level"].as_str())
            .unwrap_or("pointers");
        let mut out = render_summary(&manifest, &self.root);
        // Skeletal context: inline code for the top entries so the agent gets signatures (and,
        // with `outline`, NOT the bodies) without a separate read. `pointers` keeps it lean.
        if detail != "pointers" {
            out.push_str("\n## code\n");
            let mut shown = 0;
            for e in &manifest.entries {
                // Inline only the few top entries, tightly capped: a big `outline`/`full` dump
                // gets re-read every subsequent turn (cumulative input), so bounding it matters
                // more than completeness — the agent can read_node / retrieve again for more.
                if shown >= 4 {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(self.root.join(&e.file)) else { continue };
                // Secondary files (pulled in via the import graph, not direct query matches) are
                // CONTEXT, not the target — the agent needs their signatures to call them, not
                // their bodies. So even when `full` is requested, fold secondaries to `outline`;
                // only the primary `query-match` files come back in full. Big input-token saver,
                // and the agent can still `read_node` a secondary's body on demand.
                let primary = e.reason == "query-match";
                let body = if detail == "full" && primary {
                    content
                } else {
                    outline_for(&e.file, &content)
                };
                let body: String = body.lines().take(100).collect::<Vec<_>>().join("\n");
                // Flag the case where what we returned differs from what was asked, so the agent
                // knows the body was elided and can drill in if needed.
                let label = if detail == "full" && !primary {
                    format!("{} (outline — imported context; read_node for a body)", e.file)
                } else {
                    e.file.clone()
                };
                out.push_str(&format!("\n### {label}\n```\n{body}\n```\n"));
                shown += 1;
            }
        }
        Ok(out)
    }

    fn describe_architecture(&self, args: &Value) -> Result<String, String> {
        let nodes = build_architecture(&self.root).map_err(|e| e.to_string())?;
        Ok(format_architecture(&nodes, args["path"].as_str()))
    }

    /// Exhaustive keyword/symbol search returning self-locating node-id handles (kind + range),
    /// ranked by path-role/layer weight — the bridge between `retrieve_context` (fuzzy) and grep
    /// (literal but not handle-returning). `substring` widens exact-name matching.
    fn find_symbols(&mut self, args: &Value) -> Result<String, String> {
        let query = args["query"]
            .as_str()
            .or_else(|| args["name"].as_str())
            .ok_or("`query` is required")?
            .to_string();
        if query.trim().is_empty() {
            return Err("`query` must be non-empty".into());
        }
        let substring = args["substring"].as_bool().unwrap_or(false);
        if !index_exists(&self.root, &self.config) {
            return Err("no index — run `marksman index` first".into());
        }
        let index = self.index_data()?;
        const CAP: usize = 200;
        let (hits, total) = ci_retrieve::find_symbols(&index, &query, substring, &self.config, CAP);
        if hits.is_empty() {
            return Ok(format!(
                "(no symbols {} {query:?})",
                if substring { "containing" } else { "named" }
            ));
        }
        let shown = if total > hits.len() { format!(" (showing top {})", hits.len()) } else { String::new() };
        let mut out = format!("# {total} symbol(s) {} {query:?}{shown}\n", if substring { "containing" } else { "named" });
        for h in &hits {
            out.push_str(&format!(
                "{}  {}  ({}:L{}-{})\n",
                h.node_id, h.kind.as_str(), h.file, h.line_range[0], h.line_range[1]
            ));
        }
        Ok(out)
    }

    fn list_anchors(&mut self, args: &Value) -> Result<String, String> {
        let file = args["file"].as_str().ok_or("`file` is required")?.to_string();
        let registry = self.registry()?;
        let nodes = registry.structure(Path::new(&file)).map_err(|e| e.to_string())?;
        // Reverse edges up top: "who imports this file" is the other half of every pre-edit
        // survey (the forward half — this file's own imports — is already shown below). One
        // graph lookup here displaces a repo grep per risky edit.
        let imported_by = registry
            .entry_for(Path::new(&file))
            .and_then(|slot| registry.entry_at(slot))
            .and_then(|p| p.import_graph().ok())
            .map(|g| {
                let rev = ci_core::reverse_import_map(&g);
                rev.get(&file).cloned().unwrap_or_default()
            })
            .unwrap_or_default();
        let header = if imported_by.is_empty() {
            "imported by: (nothing in this repo)\n".to_string()
        } else {
            format!("imported by: {}\n", imported_by.join(", "))
        };
        let mut out = String::new();
        for n in &nodes {
            write_anchors(n, &mut out, 0);
        }
        if !out.is_empty() {
            out = format!("{header}{out}");
            // Imports/module decls live OUTSIDE symbol anchors, and they're half of what an
            // agent asks this tool for (bench move-ts: list_anchors on each importer, then a
            // whole-file Read anyway — just to see two import lines). Surface them up top,
            // with the file-level edit form, so one call answers both halves.
            let content = std::fs::read_to_string(self.root.join(&file)).unwrap_or_default();
            let mut tops = Vec::new();
            for (i, line) in content.lines().enumerate() {
                let t = line.trim_start();
                let is_top = t.starts_with("import ")
                    || t.starts_with("export ") && (t.contains(" from ") || t.starts_with("export * "))
                    || t.starts_with("use ")
                    || t.starts_with("pub use ")
                    || t.starts_with("mod ")
                    || t.starts_with("pub mod ")
                    || t.starts_with("from ")
                    || t.starts_with("require ")
                    || t.starts_with("#include");
                if is_top {
                    let show = if t.len() > 120 { &t[..120] } else { t };
                    tops.push(format!("  L{}: {show}", i + 1));
                    if tops.len() >= 20 {
                        break;
                    }
                }
            }
            if !tops.is_empty() {
                out = format!(
                    "file-top statements (no symbol anchor — edit via replace_text with `path` + unique `oldText`):\n{}\n{out}",
                    tops.join("\n")
                );
            }
            return Ok(out);
        }
        // No symbol anchors ≠ nothing to say. Declaration-only files (a lib.rs of `mod` lines,
        // a barrel of re-exports) are exactly where agents ask for structure — a bare
        // "(no symbols)" sent them to `find`/`cat` for the answer (bench move-rust: a 12KB
        // find dump to learn a 7-line lib.rs). Small files are inlined whole; file-level
        // statements are edited via replace_text with `path` (no symbol anchor needed).
        let content = std::fs::read_to_string(self.root.join(&file)).map_err(|e| e.to_string())?;
        let lines = content.lines().count();
        Ok(if lines <= 50 {
            format!(
                "{header}(no symbol anchors — {file} is declaration-only; its {lines} line(s) inline:)\n```\n{}\n```\nEdit these via replace_text with `path` + unique `oldText` (file-level statements sit outside symbol anchors).",
                content.trim_end()
            )
        } else {
            format!("(no symbol anchors in {file} — {lines} lines of file-level statements; read_node/Read for content, replace_text with `path` + unique `oldText` to edit)")
        })
    }

    /// Resolve a symbol reference to a provider node_id, cheapest precision first:
    ///   1. a qualified node_id (`file#Scope.name`, optionally `:body`/`:doc`/…) — used as-is,
    ///      it is unique AND self-locating (the file is before `#`), so NO retrieve is needed;
    ///   2. a bare name + a file path — resolved within that file's structure;
    ///   3. a bare name + NO usable path — searched across the INDEX: unique → resolved;
    ///      ambiguous → Err listing the candidate ids so the agent re-issues with one (one cheap
    ///      round-trip, never a full retrieve). The server disambiguates because it owns the index.
    ///
    /// Ambiguity is judged at SYMBOL granularity (every matching node id), NOT file granularity: a
    /// name reused within ONE file — two interface fields `nodeId`, an overload pair — is still
    /// ambiguous and returns candidates, so a bare-name edit never silently lands on "the first one".
    fn resolve_symbol(
        &self,
        registry: &ProviderRegistry,
        path: &str,
        reference: &str,
        op_needle: Option<&str>,
    ) -> Result<String, String> {
        if reference.contains('#') {
            // Already a node_id — but validate it NOW against the file's structure instead of
            // letting a constructed id die later as a bare "anchor not found". The common miss is
            // a nested symbol: the agent builds `file#foo` but the real id is `file#Cls.foo`.
            // On a miss, list the file's same-leaf-name ids (else all its ids) so one retry fixes it.
            let file = file_of(reference);
            let nodes = registry.structure(Path::new(file)).unwrap_or_default();
            if find_node(&nodes, reference).is_some() {
                return Ok(reference.to_string());
            }
            // Leaf name of the requested id: after the last `.` of the scope, before any `:subnode`.
            let leaf = reference
                .rsplit('#')
                .next()
                .unwrap_or(reference)
                .split(':')
                .next()
                .unwrap_or("")
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_string();
            let mut candidates = Vec::new();
            collect_ids_by_leaf(&nodes, &leaf, &mut candidates);
            if candidates.is_empty() {
                collect_ids_by_leaf(&nodes, "", &mut candidates); // no leaf match — list everything
            }
            return Err(if candidates.is_empty() {
                // A file with zero symbols usually means a wrong path — but when its LANGUAGE
                // is disabled (toolchain missing), say THAT, with the install instruction.
                match registry.disabled_reason(Path::new(file)) {
                    Some(reason) => format!("'{file}' can't be read — its language is disabled on this machine:\n{reason}"),
                    None => format!("anchor '{reference}' not found — {file} has no indexed symbols (check the path)"),
                }
            } else {
                format!(
                    "anchor '{reference}' not found in {file}. Closest ids there (nested symbols include \
                     their scope):\n{}",
                    candidates.join("\n")
                )
            });
        }
        // A path pins the file, but the name can still collide WITHIN it — collect every match there
        // and disambiguate, rather than taking the first. Only fall through to the index-wide search
        // when the name isn't defined in this file at all.
        // With a needle AND a path, candidates must actually CONTAIN the op's target before
        // they resolve — even a unique name match can't hold a file-top line, and resolving
        // it anyway guarantees an apply-time miss. Zero viable → the caller's file-level
        // fallback (path + unique oldText) takes over.
        let gate = |ids: Vec<String>| -> Result<Vec<String>, String> {
            if !path.is_empty() && op_needle.is_some_and(|n| !n.is_empty()) {
                let viable = self.viable_candidates(registry, &ids, op_needle);
                if viable.is_empty() {
                    return Err(no_containing_symbol_msg(reference));
                }
                return Ok(viable);
            }
            Ok(ids)
        };
        if !path.is_empty() {
            let nodes = registry.structure(Path::new(path)).unwrap_or_default();
            let ids = resolve_all_in(&nodes, reference);
            if !ids.is_empty() {
                let ids = gate(ids)?;
                return self.one_or_candidates(registry, reference, ids, op_needle);
            }
        }
        let files = self.files_defining(reference)?;
        let ids: Vec<String> = files
            .iter()
            .flat_map(|f| resolve_all_in(&registry.structure(Path::new(f)).unwrap_or_default(), reference))
            .collect();
        if ids.is_empty() {
            return Err(format!(
                "symbol '{reference}' not found in the index — pass a `path`, or a node id from list_anchors/retrieve_context"
            ));
        }
        let ids = gate(ids)?;
        self.one_or_candidates(registry, reference, ids, op_needle)
    }

    /// Exactly one candidate → resolve it; several → try the OP'S OWN constraint before asking:
    /// when the action carries a text the target must contain (replace_text's `oldText`,
    /// delete_in_body's line, insert_in_body's `after` anchor), a candidate whose source lacks it
    /// can't be the target — and if exactly ONE candidate qualifies, it IS the target. Asking the
    /// agent to pick would make it replay this same containment check, one round-trip later
    /// (bench T3: `replace_text k1 1.5→1.2` was ambiguous between a class field `k1 = 1.5` and an
    /// interface field `k1: number;` — only one contains "1.5"). Still ambiguous after the
    /// filter → an Err listing every candidate so the agent re-issues with one as `name`.
    /// (node ids are unique by construction — file prefix + scope — so no de-dup is needed.)
    fn one_or_candidates(
        &self,
        registry: &ProviderRegistry,
        reference: &str,
        ids: Vec<String>,
        op_needle: Option<&str>,
    ) -> Result<String, String> {
        if ids.len() > 1 {
            if let Some(needle) = op_needle.filter(|n| !n.is_empty()) {
                let hits: Vec<&String> = ids.iter().filter(|id| self.node_contains(registry, id, needle)).collect();
                if hits.len() == 1 {
                    return Ok(hits[0].clone());
                }
            }
        }
        match ids.len() {
            1 => Ok(ids.into_iter().next().unwrap()),
            _ => Err(format!(
                "'{reference}' is ambiguous ({} definitions). Re-issue with one of these as `name`:\n{}",
                ids.len(),
                ids.join("\n")
            )),
        }
    }

    /// Does the node's current source contain `needle`? (containment only — the op itself still
    /// enforces uniqueness within the node later, with its own clear error).
    fn node_contains(&self, registry: &ProviderRegistry, id: &str, needle: &str) -> bool {
        let file = file_of(id);
        let Ok(content) = std::fs::read_to_string(self.root.join(file)) else { return false };
        let nodes = registry.structure(Path::new(file)).unwrap_or_default();
        let Some(node) = find_node(&nodes, id) else { return false };
        slice_lines(&content, node.range.start_line, node.range.end_line).contains(needle)
    }

    /// Resolve a free-text `query` to a single node_id (the fuzziest addressing mode — fuse
    /// locate+edit into one call). Conservative + gated: an exact symbol-NAME token in the query
    /// resolves directly when unique; otherwise it falls back to retrieval and **only** auto-
    /// resolves when the top result is unambiguous. Before giving up on ambiguity or a miss, the
    /// OP'S OWN constraint gets a shot: when the action carries a `path` and a text the target
    /// must contain (oldText), the one symbol in that file containing it IS the target — the
    /// query was only ever a description of what the agent already pinned down precisely
    /// (bench T5: `query:"…object literal in indexer doc sections"` + path + a unique oldText
    /// drew 4 junk candidates while the oldText identified the site exactly). Only a genuinely
    /// under-determined query returns candidate ids.
    fn resolve_query(
        &mut self,
        registry: &ProviderRegistry,
        query: &str,
        path: &str,
        op_needle: Option<&str>,
    ) -> Result<String, String> {
        let id_in = |f: &str, n: &str| resolve_in(&registry.structure(Path::new(f)).unwrap_or_default(), n);
        if !index_exists(&self.root, &self.config) {
            return Err("no index — run `marksman index` first, or address by name/id".into());
        }
        // 1) an exact symbol name that appears as a token in the query.
        let index = self.index_data()?;
        let toks: std::collections::HashSet<String> =
            query.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '_').filter(|t| t.len() > 2).map(str::to_string).collect();
        let mut named: Vec<(String, String)> = index
            .symbols
            .iter()
            .filter(|s| toks.contains(&s.name.to_lowercase()))
            .map(|s| (s.file.clone(), s.name.clone()))
            .collect();
        named.sort();
        named.dedup();
        // An op that STATES its file must never be hijacked to a symbol in another file —
        // name-token matches outside `path` did exactly that (bench move-rust: a lib.rs
        // mod-decl edit resolved to src/tokenize.rs#tokenize and every retry compounded).
        // Zero in-path matches fall through to retrieval/containment and the caller's
        // file-level fallback.
        if !path.is_empty() {
            named.retain(|(f, _)| f == path);
        }
        if !named.is_empty() {
            let mut ids: Vec<String> = named.iter().filter_map(|(f, n)| id_in(f, n)).collect();
            // With a needle AND a path, even a SINGLE candidate must actually contain the
            // op's target — a name-token match that can't hold the text sent the op into a
            // guaranteed apply-time miss (bench move-rust round 5: `Store` matched, the
            // file-top `use` line didn't live in it). Zero viable → the caller's file-level
            // fallback takes over.
            if !path.is_empty() && op_needle.is_some_and(|n| !n.is_empty()) {
                ids = self.viable_candidates(registry, &ids, op_needle);
                if ids.is_empty() {
                    return Err(no_containing_symbol_msg(query));
                }
            }
            return match ids.len() {
                1 => Ok(ids.into_iter().next().unwrap()),
                _ => self.resolve_by_containment(registry, path, op_needle).ok_or_else(|| {
                    let viable = self.viable_candidates(registry, &ids, op_needle);
                    if viable.is_empty() && op_needle.is_some_and(|n| !n.is_empty()) {
                        no_containing_symbol_msg(query)
                    } else if !viable.is_empty() {
                        candidate_msg(query, &viable)
                    } else {
                        candidate_msg(query, &ids)
                    }
                }),
            };
        }
        // 2) retrieval fallback — only auto-resolve when unambiguous.
        let model = self.config.embedding_model.clone();
        let embedder = self.embedder()?;
        ensure_index_matches(&index.meta.model, index.meta.dims, &model, embedder.dim())?;
        let qvec = embedder.embed(query).map_err(|e| e.to_string())?;
        let manifest = retrieve(&self.root, query, &index, &qvec, &self.config, &RetrieveOptions { top_n: Some(3), ..Default::default() });
        let mut ids: Vec<String> = manifest
            .entries
            .iter()
            .take(2)
            .flat_map(|e| e.matched_symbols.iter().filter_map(|s| id_in(&e.file, &s.name)))
            .collect();
        ids.sort();
        ids.dedup();
        if !path.is_empty() {
            ids.retain(|id| file_of(id) == path);
            if op_needle.is_some_and(|n| !n.is_empty()) {
                ids = self.viable_candidates(registry, &ids, op_needle);
                if ids.is_empty() {
                    return Err(no_containing_symbol_msg(query));
                }
            }
        }
        match ids.len() {
            1 => Ok(ids.into_iter().next().unwrap()),
            _ => self.resolve_by_containment(registry, path, op_needle).ok_or_else(|| {
                if ids.is_empty() {
                    format!("query {query:?} resolved to no symbol — use retrieve_context to find it, then edit by name/id")
                } else {
                    let viable = self.viable_candidates(registry, &ids, op_needle);
                    if viable.is_empty() && op_needle.is_some_and(|n| !n.is_empty()) {
                        no_containing_symbol_msg(query)
                    } else if !viable.is_empty() {
                        candidate_msg(query, &viable)
                    } else {
                        candidate_msg(query, &ids)
                    }
                }
            }),
        }
    }

    /// Keep only candidate ids whose node text CONTAINS the op's own target text — a
    /// suggestion the op is guaranteed to fail on is worse than none (bench R2: a file-top
    /// `use` line token-matched symbol NAMES whose bodies couldn't contain it, and the agent
    /// obeyed the suggestion into two dead-end retries).
    fn viable_candidates(&self, registry: &ProviderRegistry, ids: &[String], needle: Option<&str>) -> Vec<String> {
        let Some(needle) = needle.filter(|n| !n.is_empty()) else { return ids.to_vec() };
        fn find<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
            for n in nodes {
                if n.id == id {
                    return Some(n);
                }
                if let Some(f) = find(&n.children, id) {
                    return Some(f);
                }
            }
            None
        }
        let mut cache: std::collections::HashMap<String, (String, Vec<Node>)> = std::collections::HashMap::new();
        ids.iter()
            .filter(|id| {
                let file = file_of(id).to_string();
                let (content, nodes) = cache.entry(file.clone()).or_insert_with(|| {
                    (
                        std::fs::read_to_string(self.root.join(&file)).unwrap_or_default(),
                        registry.structure(Path::new(&file)).unwrap_or_default(),
                    )
                });
                find(nodes, id)
                    .map(|n| slice_lines(content, n.range.start_line, n.range.end_line).contains(needle))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// The one symbol in `path` whose source contains `needle` — resolution by the op's own
    /// constraint when fuzzy addressing under-determines. Overlapping matches (a class contains
    /// everything its methods contain) reduce to the INNERMOST set; only an exactly-unique
    /// innermost hit resolves, anything else stays None so the caller's candidate error stands.
    fn resolve_by_containment(&self, registry: &ProviderRegistry, path: &str, needle: Option<&str>) -> Option<String> {
        let needle = needle.filter(|n| !n.is_empty())?;
        if path.is_empty() {
            return None;
        }
        let content = std::fs::read_to_string(self.root.join(path)).ok()?;
        let nodes = registry.structure(Path::new(path)).unwrap_or_default();
        let hits: Vec<&Node> = nodes
            .iter()
            .filter(|n| n.name.is_some())
            .filter(|n| slice_lines(&content, n.range.start_line, n.range.end_line).contains(needle))
            .collect();
        let innermost: Vec<&&Node> = hits
            .iter()
            .filter(|a| {
                !hits.iter().any(|b| {
                    b.id != a.id
                        && a.range.start_line <= b.range.start_line
                        && b.range.end_line <= a.range.end_line
                })
            })
            .collect();
        match innermost.as_slice() {
            [only] => Some(only.id.clone()),
            _ => None,
        }
    }

    /// Repo-relative files that define a symbol with this exact (bare) name, from the index.
    fn files_defining(&self, name: &str) -> Result<Vec<String>, String> {
        if !index_exists(&self.root, &self.config) {
            return Ok(vec![]);
        }
        let index = self.index_data()?;
        let mut files: Vec<String> = index.symbols.iter().filter(|s| s.name == name).map(|s| s.file.clone()).collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    /// Drill-down: the full source + metadata of ONE anchor (a symbol or its `:body`/`:param`/
    /// `:return`/`:doc` sub-node). Address by `id` (a node id — self-locating, no `file` needed),
    /// or by `name` (+ optional `file`; resolved via the index when `file` is omitted).
    fn read_node(&mut self, args: &Value) -> Result<String, String> {
        let registry = self.registry()?;
        let id = if let Some(id) = args["id"].as_str() {
            id.to_string()
        } else if let Some(name) = args["name"].as_str() {
            self.resolve_symbol(&registry, args["file"].as_str().unwrap_or(""), name, None)?
        } else {
            return Err("provide `id` (a node id from list_anchors) or `name`".into());
        };
        let file = file_of(&id).to_string();
        let nodes = registry.structure(Path::new(&file)).map_err(|e| e.to_string())?;
        let node = find_node(&nodes, &id).ok_or_else(|| {
            // Same near-miss help as resolve_symbol: a constructed id usually missed the scope.
            let leaf = id.rsplit('#').next().unwrap_or(&id).split(':').next().unwrap_or("").rsplit('.').next().unwrap_or("");
            let mut candidates = Vec::new();
            collect_ids_by_leaf(&nodes, leaf, &mut candidates);
            if candidates.is_empty() {
                collect_ids_by_leaf(&nodes, "", &mut candidates);
            }
            if candidates.is_empty() {
                format!("anchor '{id}' not found in {file}")
            } else {
                format!("anchor '{id}' not found in {file}. Closest ids there:\n{}", candidates.join("\n"))
            }
        })?;
        let content = std::fs::read_to_string(self.root.join(&file)).map_err(|e| e.to_string())?;
        let text = slice_lines(&content, node.range.start_line, node.range.end_line);
        let kind = match &node.kind {
            NodeKind::Symbol(k) => k.as_str().to_string(),
            NodeKind::Syntax(s) => s.clone(),
        };
        Ok(format!(
            "{kind} {}  ({file}:L{}-{})\n```\n{text}\n```",
            node.name.as_deref().unwrap_or(&id),
            node.range.start_line,
            node.range.end_line,
        ))
    }

    /// The facade surface's single read tool: `mode` picks the underlying handler, args are
    /// mapped onto the handler's own names. Pure dispatch — behavior identical to the six-tool
    /// surface, which is what makes the facade a single-variable ablation.
    fn inspect(&mut self, args: &Value) -> Result<String, String> {
        let mode = args["mode"].as_str().unwrap_or("");
        match mode {
            "search" => {
                let mut a = json!({"task": args["query"]});
                if !args["detailLevel"].is_null() {
                    a["detailLevel"] = args["detailLevel"].clone();
                }
                self.retrieve_context(&a)
            }
            "symbol" => self.find_symbols(&json!({"query": args["query"], "substring": args["substring"]})),
            "file" => self.list_anchors(&json!({"file": args["file"]})),
            "node" => self.read_node(&json!({"id": args["id"], "name": args["name"], "file": args["file"]})),
            "map" => self.describe_architecture(&json!({"path": args["path"]})),
            other => Err(format!(
                "unknown inspect mode {other:?} — one of: {}",
                INSPECT_MODES.join(", ")
            )),
        }
    }

    fn apply_edits(&mut self, args: &Value) -> Result<String, String> {
        let dry_run = args["dryRun"].as_bool().unwrap_or(false);
        let actions = args["actions"].as_array().ok_or("`actions` array is required")?.clone();
        let registry = self.registry()?;

        let mut ops = Vec::new();
        // Pre-edit text of every node a replace_node/set_body op overwrites, captured while
        // the disk is pristine; appended to gate REJECTS (see replaced_extent_note).
        let mut replaced_notes: Vec<String> = Vec::new();
        // Symbol ids whose post-commit block is echoed back in the SUCCESS response.
        let mut echo_ids: Vec<String> = Vec::new();
        for (ai, a) in actions.iter().enumerate() {
            // Reject unknown fields UP FRONT: a misspelled field (`old_text`, `after`) would
            // otherwise be silently dropped — and for insert_in_body a dropped `after` doesn't
            // error, it silently changes WHERE the code lands (end of body). A clear one-round-trip
            // correction beats a wrong edit that type-checks.
            if let Some(obj) = a.as_object() {
                const KNOWN: [&str; 9] = ["action", "name", "query", "path", "value", "oldText", "newText", "target", "file"];
                for k in obj.keys() {
                    if !KNOWN.contains(&k.as_str()) {
                        let hint = if k == "after" {
                            " — did you mean `oldText` (it holds the insert-after anchor)?".to_string()
                        } else {
                            KNOWN
                                .iter()
                                .find(|known| known.eq_ignore_ascii_case(&k.replace(['_', '-'], "")))
                                .map(|known| format!(" — did you mean `{known}`?"))
                                .unwrap_or_default()
                        };
                        return Err(format!(
                            "action #{ai}: unknown field `{k}`{hint} Allowed fields: {}",
                            KNOWN.join(", ")
                        ));
                    }
                }
            }
            let act = a["action"].as_str().unwrap_or("");
            // `file` is accepted as an alias for `path` — agents guess it constantly (bench:
            // one retry per guess), and the two words mean the same thing here.
            let mut path = a["path"].as_str().or_else(|| a["file"].as_str()).unwrap_or("").to_string();
            let mut name = a["name"].as_str().map(str::to_string);
            // The op's containment constraint, for auto-disambiguation of fuzzy addressing:
            // replace_text/delete_in_body's oldText and insert_in_body's `after` anchor must all
            // occur in the target's source (see one_or_candidates / resolve_by_containment).
            let op_needle = match act {
                "replace_text" | "delete_in_body" | "insert_in_body" => a["oldText"].as_str(),
                _ => None,
            };
            // `query` — the fuzziest target: resolve a free-text description to a node_id via the
            // index/retrieval (fuse locate+edit). Only when no explicit name/id was given.
            let mut resolution_err: Option<String> = None;
            if name.is_none() {
                if let Some(q) = a["query"].as_str() {
                    match self.resolve_query(&registry, q, &path, op_needle) {
                        Ok(id) => name = Some(id),
                        Err(e) => resolution_err = Some(e),
                    }
                }
            }
            // For a symbol-targeting action, resolve the reference to a node_id UP FRONT through
            // the addressing model (id ≫ name-in-file ≫ name-in-index). This is what lets the
            // agent edit by name with no prior retrieve — the index supplies the file — and turns
            // a same-name collision into a candidate list instead of an error. File ops
            // (move/create/delete) carry no symbol, so they're left untouched.
            let symbol_action = matches!(
                act,
                "rename" | "replace_node" | "replace_text" | "set_body" | "insert_before"
                    | "insert_in_body" | "delete_in_body" | "insert_member" | "add_parameter" | "set_return_type"
            );
            // add_symbol addresses a FILE, but the agent often knows only a symbol or module
            // NAME inside it ("add a helper to the parsing module") — resolve name/query
            // through the same addressing model as every other action and take its file.
            // Without this, every add_symbol pays a locate turn that the resolution layer
            // already knows the answer to (measured: both add-symbol bench cells ran
            // inspect → apply where apply alone suffices).
            if act == "add_symbol" && path.is_empty() && resolution_err.is_none() {
                if let Some(reference) = name.as_deref() {
                    match self.resolve_symbol(&registry, "", reference, None) {
                        Ok(id) => path = file_of(&id).to_string(),
                        Err(e) => resolution_err = Some(e),
                    }
                }
            }
            if symbol_action && resolution_err.is_none() {
                if let Some(reference) = name.as_deref() {
                    match self.resolve_symbol(&registry, &path, reference, op_needle) {
                        Ok(id) => name = Some(id),
                        Err(e) => {
                            resolution_err = Some(e);
                            name = None;
                        }
                    }
                }
            }
            // FILE-LEVEL replace_text: text outside every symbol anchor (imports, `mod`
            // declarations, file-top statements) is addressed by `path` + a UNIQUE `oldText` —
            // either because no symbol was named at all, or because symbol resolution failed.
            // Same VFS + gate as any other op; uniqueness in the file is the whole address.
            if act == "replace_text" && !path.is_empty() && name.is_none() {
                let old = a["oldText"].as_str().unwrap_or("");
                let new_text = a["newText"].as_str().unwrap_or("");
                if !old.is_empty() {
                    let content = std::fs::read_to_string(self.root.join(&path))
                        .map_err(|e| format!("action #{ai}: cannot read {path}: {e}"))?;
                    match content.matches(old).count() {
                        1 => {
                            ops.push(ci_core::EditOp::ReplaceInFile {
                                path: path.clone().into(),
                                old_text: old.to_string(),
                                new_text: new_text.to_string(),
                            });
                            continue;
                        }
                        0 => {
                            return Err(resolution_err.unwrap_or_else(|| {
                                format!("action #{ai}: oldText not found in {path} — it must match the file's current text exactly")
                            }))
                        }
                        n => {
                            return Err(format!(
                                "action #{ai}: oldText occurs {n} times in {path} — extend it until unique (file-level edit), or address a symbol by name"
                            ))
                        }
                    }
                }
            }
            if let Some(e) = resolution_err {
                return Err(e);
            }
            if wants_original_extent_note(act) {
                if let Some(id) = name.as_deref().filter(|n| n.contains('#')) {
                    if let Some(note) = replaced_extent_note(&self.root, &registry, ai, act, id) {
                        replaced_notes.push(note);
                    }
                }
            }
            // Symbol-anchored CONTENT edits get their block echoed back post-commit (see
            // post_edit_echo) — renames/moves have the scan, file ops have nothing to echo.
            if symbol_action && act != "rename" {
                if let Some(id) = name.as_deref().filter(|n| n.contains('#')) {
                    if !echo_ids.contains(&id.to_string()) {
                        echo_ids.push(id.to_string());
                    }
                }
            }
            // add_symbol appends at top level, so its committed id is `path#Name` — the name
            // read off the snippet's leading declaration. No name derivable ⇒ just no echo.
            if act == "add_symbol" && !path.is_empty() {
                if let Some(n) = a["value"].as_str().and_then(ci_edit::leading_symbol_name) {
                    let id = format!("{path}#{n}");
                    if !echo_ids.contains(&id) {
                        echo_ids.push(id);
                    }
                }
            }
            let action = Action {
                path,
                action: act.to_string(),
                target: a["target"].as_str().map(str::to_string),
                name,
                value: a["value"].as_str().map(str::to_string),
                old_text: a["oldText"].as_str().map(str::to_string),
                new_text: a["newText"].as_str().map(str::to_string),
            };
            // `name` is already a node_id after resolution; pass node_ids through unchanged, and
            // fall back to name-in-file resolution for any caller that didn't pre-resolve.
            let resolve = |p: &str, _t: Option<&str>, n: Option<&str>| {
                n.and_then(|nm| {
                    if nm.contains('#') {
                        Some(nm.to_string())
                    } else {
                        resolve_in(&registry.structure(Path::new(p)).unwrap_or_default(), nm)
                    }
                })
            };
            ops.push(action_to_op(&action, resolve).map_err(|e| e.to_string())?);
        }

        // Dispatch STRICTLY per file's language — never fall back to another language's provider
        // (a `.ts` edit handled by, say, the Python fallback would apply garbage structurally +
        // ungated). A batch may legally MIX languages (a multilanguage repo: one batch renaming a
        // Rust and a TS symbol), so ops are GROUPED per provider, every group is gated first
        // (dry-run), and only when all gates pass does anything commit — cross-language batches
        // stay all-or-nothing. If an op's language has no active provider, say so loudly. A truly
        // path-less batch uses the first provider.
        if registry.providers().next().is_none() {
            return Err("no language provider available for this repo".into());
        }
        // Rename facts (extension + old leaf name), gathered while we still own `ops`: if any
        // group turns out UNGATED, the server re-verifies these repo-wide itself and puts the
        // evidence in the response (see `scan_word`) — the agent must never burn turns
        // grepping to check a rename the server can check in milliseconds.
        let mut renames: Vec<(String, String, String)> = Vec::new(); // (ext, old leaf, new name)
        for op in &ops {
            if let ci_core::EditOp::Rename { node_id, new_name } = op {
                let file = file_of(node_id);
                let leaf = node_id
                    .rsplit('#')
                    .next()
                    .unwrap_or(node_id)
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let ext = Path::new(file).extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
                if !leaf.is_empty() && !ext.is_empty() && !renames.iter().any(|(e, o, _)| e == &ext && o == &leaf) {
                    renames.push((ext, leaf, new_name.clone()));
                }
            }
        }
        let mut groups: Vec<(usize, Vec<ci_core::EditOp>)> = Vec::new();
        let mut text_ops: Vec<ci_core::EditOp> = Vec::new();
        for op in ops {
            let slot = match op_file(&op) {
                Some(f) => match registry.entry_for(Path::new(&f)) {
                    Some(slot) => slot,
                    None => {
                        // The config/manifest gap, reopened NARROWLY: a non-provider TEXT file
                        // (Cargo.toml, package.json, …) accepts file-level replace_text ONLY —
                        // honestly ungated (syntax-checked at most, never type-checked), and
                        // committed only after every code group's gate passes, so batch
                        // atomicity holds for rename-a-crate / add-a-dependency flows. No
                        // structured TOML/JSON editing (deliberately reverted in faad1c9), no
                        // create/move/delete here, and a DISABLED language never degrades to
                        // this path — fix the toolchain, don't ungate the edit.
                        let disabled = registry.disabled_reason(Path::new(&f));
                        if disabled.is_none()
                            && matches!(op, ci_core::EditOp::ReplaceInFile { .. })
                            && std::fs::read_to_string(self.root.join(&f)).is_ok()
                        {
                            text_ops.push(op);
                            continue;
                        }
                        // The dependency layer: when the language is present but its toolchain
                        // is missing, hand over the exact install instruction — not a shrug.
                        return Err(match disabled {
                            Some(reason) => format!("no provider for '{f}' — its language is disabled on this machine:\n{reason}"),
                            None if matches!(op, ci_core::EditOp::ReplaceInFile { .. }) => format!(
                                "no language provider for '{f}', and it isn't a readable text file — nothing can edit it"
                            ),
                            None => format!(
                                "no language provider for '{f}' — structural ops need a provider; a config/text file takes replace_text with `path` + a unique `oldText` (file-level, applied ungated)"
                            ),
                        });
                    }
                },
                None => 0, // path-less op: first provider, as before
            };
            match groups.iter_mut().find(|(s, _)| *s == slot) {
                Some((_, v)) => v.push(op),
                None => groups.push((slot, vec![op])),
            }
        }
        // Manifest edits gate FIRST: a malformed TOML/JSON result must reject the batch before
        // any code group commits (atomicity) — and commit LAST (below), after every code gate
        // has passed.
        if !text_ops.is_empty() {
            self.apply_text_ops(&text_ops, false).map_err(|e| format!("rejected — nothing written:\n{e}"))?;
        }
        if groups.is_empty() {
            if text_ops.is_empty() {
                return Ok("Applied 0 edit(s); no file changes were necessary.".into());
            }
            // Pure config/text batch: no provider, no compiler — say exactly what was checked.
            let (files, receipt) = self.apply_text_ops(&text_ops, !dry_run)?;
            return Ok(format!(
                "✓ Applied {} file-level edit(s){}; {} config/text file(s) changed. gated: false — no compiler covers \
                 these files: TOML/JSON syntax is verified (the file still parses), NEVER type-checked. The lines as \
                 committed:\n{receipt}Files changed:\n{}",
                text_ops.len(),
                if dry_run { " (dry run — nothing written yet)" } else { "" },
                files.len(),
                files.iter().map(|p| format!("  {p}")).collect::<Vec<_>>().join("\n"),
            ));
        }
        let provider_of = |slot: usize| registry.entry_at(slot).expect("slot from entry_for");

        let all_gated = groups.iter().all(|(slot, _)| provider_of(*slot).gated());
        let opts = EditOpts { write: !dry_run, dry_run, tsconfig: None };
        let res = if groups.len() == 1 {
            provider_of(groups[0].0).apply_edits(&groups[0].1, &opts).map_err(|e| e.to_string())?
        } else {
            // Multi-language batch. Gate phase: every group dry-runs; the first rejection wins
            // and NOTHING has been written. (Feedback op numbers are within that language's
            // sub-batch.) Languages can't type-depend on each other, so a commit in one can't
            // change another's gate verdict.
            let gate = EditOpts { write: false, dry_run: true, tsconfig: None };
            let rejection = groups
                .iter()
                .map(|(slot, gops)| provider_of(*slot).apply_edits(gops, &gate).map_err(|e| e.to_string()))
                .find(|r| !matches!(r, Ok(ci_core::CommitResult::Ok { .. })))
                .transpose()?;
            match rejection {
                Some(rej) => rej,
                None => {
                    // Every gate passed — commit each group (skipped on dry_run) and merge.
                    let mut applied = 0usize;
                    let mut changed: Vec<PathBuf> = Vec::new();
                    let mut preexisting: Vec<ci_core::Diag> = Vec::new();
                    let mut redundant = 0usize;
                    let mut rewrites: Vec<String> = Vec::new();
                    for (gi, (slot, gops)) in groups.iter().enumerate() {
                        match provider_of(*slot).apply_edits(gops, &opts).map_err(|e| e.to_string())? {
                            ci_core::CommitResult::Ok { applied_ops, changed_files, preexisting_in_radius, redundant_ops, rewrite_summary, .. } => {
                                applied += applied_ops;
                                changed.extend(changed_files);
                                preexisting.extend(preexisting_in_radius);
                                redundant += redundant_ops;
                                if !rewrite_summary.is_empty() {
                                    rewrites.push(rewrite_summary);
                                }
                            }
                            ci_core::CommitResult::Rejected { feedback, .. } => {
                                // Gate passed but the write-run rejected (nondeterministic
                                // tooling). Earlier groups ARE committed — report honestly.
                                return Err(format!(
                                    "partial: {applied} edit(s) already committed ({} file(s): {}) before sub-batch #{gi} was rejected:\n{feedback}",
                                    changed.len(),
                                    changed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
                                ));
                            }
                        }
                    }
                    changed.sort();
                    changed.dedup();
                    ci_core::CommitResult::Ok { applied_ops: applied, changed_files: changed, repair_rounds: 0, preexisting_in_radius: preexisting, redundant_ops: redundant, rewrite_summary: rewrites.join("\n") }
                }
            }
        };
        // Keep the index true: after a real (written) commit, incrementally reindex the changed
        // files so the same session's next retrieve_context/list_anchors sees the new state. A
        // reindex hiccup must NOT fail the (already-committed) edit — log and carry on stale.
        if let ci_core::CommitResult::Ok { changed_files, .. } = &res {
            if !dry_run && !changed_files.is_empty() {
                if let Err(e) = self.reindex_after_edit(changed_files) {
                    eprintln!("[marksman-mcp] post-edit reindex failed (index may be stale until next `index`): {e}");
                }
            }
        }
        // Manifest edits commit LAST — only after the code result is a committed Ok — so a gate
        // rejection anywhere leaves the manifests untouched (batch atomicity). They already
        // passed their own gate pass above; a failure here is I/O-level and reported as partial.
        let text_section = if !text_ops.is_empty() && matches!(res, ci_core::CommitResult::Ok { .. }) {
            let (files, receipt) = self.apply_text_ops(&text_ops, !dry_run).map_err(|e| {
                format!("partial: the code edits are committed, but the config/text edit then failed: {e}")
            })?;
            Some(format!(
                "\nconfig/text file(s) also changed (gated: false — no compiler covers them; TOML/JSON syntax verified, \
                 NEVER type-checked): {}\nthe lines as committed:\n{receipt}",
                files.join(", ")
            ))
        } else {
            None
        };
        let reply: Result<String, String> = match res {
            ci_core::CommitResult::Ok { applied_ops, changed_files, .. } if changed_files.is_empty() => {
                Ok(format!(
                    "Applied {applied_ops} edit(s){}; no file changes were necessary.",
                    if dry_run { " (dry run)" } else { "" }
                ))
            }
            ci_core::CommitResult::Ok { applied_ops, changed_files, ref preexisting_in_radius, .. }
                if all_gated && !preexisting_in_radius.is_empty() =>
            {
                // Committed AND legal (pre-existing breakage never blocks an edit), but the
                // radius is NOT clean — claiming "COMPLETE, do not verify" here sent agents
                // away from errors one `use`-path fix away (bench move-rust round 4).
                let mut sites: Vec<String> = preexisting_in_radius
                    .iter()
                    .take(12)
                    .map(|d| format!("  {}:{} {}", d.file, d.line, d.message))
                    .collect();
                if preexisting_in_radius.len() > 12 {
                    sites.push(format!("  … and {} more", preexisting_in_radius.len() - 12));
                }
                let echo = if !dry_run { self.post_edit_echo(&registry, &echo_ids) } else { None };
                Ok(format!(
                    "✓ Applied {applied_ops} edit(s){}; {} file(s) changed; no NEW errors introduced — but {} PRE-EXISTING \
                     error(s) remain in the touched files (they predate this batch and did not block it). Fix them next or \
                     the build stays broken:\n{}{}\nFiles changed:\n{}",
                    if dry_run { " (dry run — nothing written yet)" } else { "" },
                    changed_files.len(),
                    preexisting_in_radius.len(),
                    sites.join("\n"),
                    echo.unwrap_or_default(),
                    changed_files.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"),
                ))
            }
            ci_core::CommitResult::Ok { applied_ops, changed_files, redundant_ops, ref rewrite_summary, .. } if all_gated => {
                // "COMPLETE — do not grep" is true for CODE references (the compiler renamed
                // them) and false for comments/strings/docs, which no semantic rename touches.
                // Same §5 law as the ungated tier: run the scan server-side and hand over the
                // evidence with copyable fixes, instead of overclaiming and letting the stale
                // mention surface later (bench type-rename: a doc-comment kept the old name
                // while the response forbade the grep that would have caught it).
                let (auto, scan, echo) = if !dry_run {
                    // Order matters: comment-only mentions are fixed FIRST (same gate), so the
                    // scan that follows reports only what genuinely needs the agent's judgment.
                    let applied = self.auto_update_comment_mentions(&registry, &renames);
                    let auto = if applied.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "\ncomment/doc mentions updated to follow the rename (committed through the same gate):\n{}\n",
                            applied.join("\n")
                        )
                    };
                    (auto, self.rename_scan_section(&registry, &renames, true), self.post_edit_echo(&registry, &echo_ids))
                } else {
                    (String::new(), None, None)
                };
                // The redundancy receipt: the agent's insurance helpers were accepted as
                // no-ops because the structural op's own automation got there first. Saying
                // so — with the count — is what converts the hedge into trust for the NEXT
                // batch; the description's completeness claim is assertion, this is evidence.
                // The rewrite receipt: the exact lines the rename/move automation changed,
                // computed from the VFS while both states existed. This is the survey the
                // agent would otherwise run BEFORE the op — shown after, as proof.
                let rewrites = if rewrite_summary.is_empty() {
                    String::new()
                } else {
                    format!("\nwhat the rename/move rewrote (complete — every changed line):\n{rewrite_summary}")
                };
                let receipt = if redundant_ops > 0 {
                    format!(
                        "\nNote: {redundant_ops} of your {applied_ops} action(s) were REDUNDANT — the batch's rename/move \
                         automation had already produced their exact end state (accepted as no-ops, nothing double-applied). \
                         The bare rename/move_file alone was sufficient; the helper actions only cost output tokens."
                    )
                } else {
                    String::new()
                };
                Ok(format!(
                    "✓ Applied {applied_ops} edit(s){}; {} file(s) changed; type-checked clean — no new type errors anywhere, \
                     including files that import what changed. rename/move already updated every reference/import across the \
                     whole codebase, so this change is COMPLETE — do not grep, re-read, or hand-edit call sites to verify.{receipt}{rewrites}{auto}{}{}\nFiles changed:\n{}",
                    if dry_run { " (dry run — nothing written yet)" } else { "" },
                    changed_files.len(),
                    scan.unwrap_or_default(),
                    echo.unwrap_or_default(),
                    changed_files.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"),
                ))
            }
            // Ungated (tree-sitter fallback — or a mixed batch touching one such language):
            // structural edit, NOT type-checked. Honesty stays, but the server does the
            // verification IT can do — a repo-wide rename scan — and hands the evidence over,
            // instead of telling the agent to go verify (which measurably costs 2-4 turns of
            // grep/read per task: the T8 bench arm lost to baseline on exactly that).
            ci_core::CommitResult::Ok { applied_ops, changed_files, ref rewrite_summary, .. } => {
                let rewrites = if rewrite_summary.is_empty() {
                    String::new()
                } else {
                    format!("\nwhat the rename/move rewrote (from the staged changes, as committed):\n{rewrite_summary}")
                };
                let scan = if !dry_run {
                    let applied = self.auto_update_comment_mentions(&registry, &renames);
                    let auto = if applied.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "\ncomment/doc mentions updated to follow the rename (committed through the same pipeline):\n{}\n",
                            applied.join("\n")
                        )
                    };
                    format!(
                        "{auto}{}{}",
                        self.rename_scan_section(&registry, &renames, false)
                            .unwrap_or_else(|| "\nRun the project's own checks if correctness is uncertain.\n".to_string()),
                        self.post_edit_echo(&registry, &echo_ids).unwrap_or_default()
                    )
                } else {
                    "\nRun the project's own checks if correctness is uncertain.\n".to_string()
                };
                Ok(format!(
                    "✓ Applied {applied_ops} structural edit(s){}; {} file(s) changed. gated: false — syntax-checked \
                     (the result parses; edits introducing syntax errors are rejected) but NOT type-verified: no \
                     type-checker is wired for the edited language(s).{rewrites}{scan}Files changed:\n{}",
                    if dry_run { " (dry run — nothing written yet)" } else { "" },
                    changed_files.len(),
                    changed_files.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"),
                ))
            }
            ci_core::CommitResult::Rejected { feedback, .. } => {
                let mut msg = format!("rejected — nothing written:\n{feedback}");
                if !replaced_notes.is_empty() {
                    // The ORIGINAL text is the missing half of a mis-scoped replacement reject:
                    // the diagnostics show the broken AFTER, this shows the intact BEFORE —
                    // enough to compose the retry in the SAME response, no read_node needed.
                    msg.push_str(&format!(
                        "\n\nOriginal target(s), UNCHANGED on disk — compose the retry against these (no need to re-read):\n{}",
                        replaced_notes.join("\n")
                    ));
                }
                Err(msg)
            }
        };
        let mut reply = reply?;
        if let Some(sec) = text_section {
            reply.push_str(&sec);
        }
        Ok(reply)
    }

    /// File-level replace_text on NON-PROVIDER text files (Cargo.toml, package.json, …) — the
    /// narrow config/manifest surface: `path` + unique `oldText`, honestly ungated. Syntax is
    /// checked where the format has a parser (TOML/JSON must still parse post-edit); nothing is
    /// ever type-checked, and there is NO structured TOML/JSON editing (deliberately reverted
    /// in faad1c9). `write:false` is the gate pass (validate only). Returns the changed files
    /// and a −/+ receipt of the lines exactly as committed.
    fn apply_text_ops(&self, text_ops: &[ci_core::EditOp], write: bool) -> Result<(Vec<String>, String), String> {
        let mut files: Vec<(String, Vec<(&String, &String)>)> = Vec::new();
        for op in text_ops {
            if let ci_core::EditOp::ReplaceInFile { path, old_text, new_text } = op {
                let rel = path.to_string_lossy().replace('\\', "/");
                match files.iter_mut().find(|(p, _)| *p == rel) {
                    Some((_, v)) => v.push((old_text, new_text)),
                    None => files.push((rel, vec![(old_text, new_text)])),
                }
            }
        }
        let mut changed = Vec::new();
        let mut receipt = String::new();
        for (rel, edits) in &files {
            let orig = std::fs::read_to_string(self.root.join(rel)).map_err(|e| format!("cannot read {rel}: {e}"))?;
            let mut content = orig.clone();
            receipt.push_str(&format!("  {rel}:\n"));
            for (old, new) in edits {
                match content.matches(old.as_str()).count() {
                    1 => {
                        content = content.replacen(old.as_str(), new, 1);
                        for l in old.lines() {
                            receipt.push_str(&format!("    - {l}\n"));
                        }
                        for l in new.lines() {
                            receipt.push_str(&format!("    + {l}\n"));
                        }
                    }
                    0 if !new.is_empty() && content.contains(new.as_str()) => {
                        receipt.push_str("    (already satisfied — the file already carries the newText)\n");
                    }
                    0 => {
                        return Err(format!(
                            "oldText not found in {rel} — it must match the file's current text exactly"
                        ))
                    }
                    n => return Err(format!("oldText occurs {n} times in {rel} — extend it until unique")),
                }
            }
            // The syntax gate — the honest maximum for a manifest. Formats without a parser
            // here are applied as plain text (the reply already says gated: false).
            match Path::new(rel).extension().and_then(|e| e.to_str()) {
                Some("json") => {
                    serde_json::from_str::<serde_json::Value>(&content).map_err(|e| {
                        format!("{rel} would no longer parse as JSON after this edit: {e}")
                    })?;
                }
                Some("toml") => {
                    content.parse::<toml::Table>().map_err(|e| {
                        format!("{rel} would no longer parse as TOML after this edit: {e}")
                    })?;
                }
                _ => {}
            }
            if content != orig {
                if write {
                    std::fs::write(self.root.join(rel), &content).map_err(|e| format!("cannot write {rel}: {e}"))?;
                }
                changed.push(rel.clone());
            }
        }
        Ok((changed, receipt))
    }

    /// Incrementally reindex `changed` after a committed edit and persist, so the same session's
    /// next retrieve_context/list_anchors/name-resolution sees the new state. Reuses the on-disk
    /// index (load → `update_index` → atomic save). No-op when there's no index yet or nothing
    /// changed; `root`/`config` are cloned so the embedder borrow doesn't alias `self`.
    fn reindex_after_edit(&mut self, changed: &[PathBuf]) -> Result<(), String> {
        let root = self.root.clone();
        let config = self.config.clone();
        if changed.is_empty() || !index_exists(&root, &config) {
            return Ok(());
        }
        let registry = self.registry()?;
        let data = load_index(&root, &config).map_err(|e| e.to_string())?;
        let changed_rel: Vec<String> =
            changed.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
        let embedder = self.embedder()?;
        let dim = embedder.dim();
        let updated = ci_build::update_index(
            &root,
            &registry,
            |t| embedder.embed(t).unwrap_or_else(|_| vec![0.0; dim]),
            data,
            &changed_rel,
        )
        .map_err(|e| e.to_string())?;
        save_index(&root, &config, &updated).map_err(|e| e.to_string())?;
        // We already hold the freshest index — seed the cache with it so the next tool call
        // (often a name resolution right after the edit) doesn't re-read what we just wrote.
        let mtime = std::fs::metadata(index_dir(&root, &config).join("index.pb")).and_then(|m| m.modified()).ok();
        if let (Ok(mut cache), Some(m)) = (self.index_cache.lock(), mtime) {
            *cache = Some((m, Arc::new(updated)));
        }
        Ok(())
    }

    /// Post-commit rename verification (docs/benchmarks.md §5): scan EVERY file of the renamed
    /// symbol's language for the old name and hand the evidence over, each hit with a
    /// ready-to-copy `replace_text` fix anchored to the POST-rename symbol — the agent's own
    /// attempts anchor by the old name (gone) and burn turns discovering that. `None` when the
    /// batch renamed nothing. On the GATED tier every hit is by construction a comment/string/
    /// doc mention (the compiler renamed the code); ungated, hits may be either — the guidance
    /// differs, the mechanism is one.
    fn rename_scan_section(
        &self,
        registry: &ProviderRegistry,
        renames: &[(String, String, String)],
        gated: bool,
    ) -> Option<String> {
        if renames.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        let mut any_hits = false;
        for (ext, old, new) in renames {
            let hits = scan_word(&self.root, ext, old);
            if hits.is_empty() {
                lines.push(format!("  '{old}': no remaining occurrences in any .{ext} file ✓"));
                continue;
            }
            any_hits = true;
            lines.push(format!("  '{old}': {} line(s) still mention it —", hits.len()));
            for (rel, ln, text) in &hits {
                lines.push(format!("    {rel}:{ln}: {text}"));
                // Full-line oldText for uniqueness; target:doc when the line is a doc comment.
                if let Some((anchor, target)) = enclosing_anchor(registry, rel, *ln) {
                    let mut fix = json!({
                        "action": "replace_text",
                        "name": anchor,
                        "oldText": text,
                        "newText": text.replace(old.as_str(), new.as_str()),
                    });
                    if let Some(t) = target {
                        fix["target"] = json!(t);
                    }
                    lines.push(format!("      fix (ready to copy): {fix}"));
                }
            }
        }
        let guidance = match (gated, any_hits) {
            (true, true) => {
                "Code references are COMPLETE (type-checked) — the lines above are comment/string/doc \
                 mentions, which no compiler rename touches. Stale prose misleads the next reader: \
                 update these too unless the user explicitly wants the old wording kept. Re-issue each \
                 `fix` action VERBATIM — all of them in ONE apply_edits batch. Do NOT grep or re-read \
                 to double-check."
            }
            (true, false) => {
                "Even comments and strings carry no stale mention — the rename(s) are COMPLETE \
                 everywhere. Do NOT grep, re-read, or run checks to verify."
            }
            (false, true) => {
                "The scan above already re-checked the whole repo — do NOT grep, re-read, or list_anchors \
                 to verify the rename(s). Code references MUST be fixed; comment/doc mentions SHOULD \
                 follow the rename too (stale prose misleads the next reader) unless the user wants the \
                 old wording kept. To fix any line, re-issue its `fix` action VERBATIM — all of them in \
                 ONE apply_edits batch."
            }
            (false, false) => {
                "The scan above already re-checked the whole repo — the rename(s) are COMPLETE. Do NOT \
                 grep, re-read, or run checks to verify."
            }
        };
        Some(format!(
            "\nrename verification (server-side scan of EVERY file of that language):\n{}\n{guidance}\n",
            lines.join("\n")
        ))
    }

    /// After a committed rename, textual mentions of the old name on WHOLE-LINE COMMENTS
    /// (`//`, `///`, `/*`, `* `, `#`, …) are updated automatically through the same gated
    /// pipeline — when the logic's names change, prose that describes them is part of the
    /// diff (Davi's rule), and a comment-only line cannot change behavior, so there is no
    /// judgment call to delegate. Everything else (string literals, trailing comments on
    /// code lines, ambiguous lines) is deliberately LEFT for the scan to report with
    /// copyable fixes — those can alter behavior or need intent. Returns one description
    /// line per applied update; the subsequent scan then only lists what genuinely remains.
    fn auto_update_comment_mentions(
        &mut self,
        registry: &ProviderRegistry,
        renames: &[(String, String, String)],
    ) -> Vec<String> {
        const COMMENT_PREFIXES: [&str; 8] = ["///", "//!", "//", "/*", "* ", "*/", "#", "--"];
        let mut by_slot: std::collections::HashMap<usize, Vec<ci_core::EditOp>> = std::collections::HashMap::new();
        let mut descs: Vec<(usize, String)> = Vec::new();
        for (ext, old, new) in renames {
            for (rel, ln, _) in scan_word(&self.root, ext, old) {
                let Ok(content) = std::fs::read_to_string(self.root.join(&rel)) else { continue };
                let Some(raw) = content.lines().nth(ln.saturating_sub(1) as usize) else { continue };
                let t = raw.trim_start();
                let is_comment = !t.starts_with("#!") && COMMENT_PREFIXES.iter().any(|p| t.starts_with(p));
                if !is_comment || content.matches(raw).count() != 1 {
                    continue; // not provably a comment-only line (or ambiguous): the scan lists it
                }
                let Some(slot) = registry.entry_for(Path::new(&rel)) else { continue };
                by_slot.entry(slot).or_default().push(ci_core::EditOp::ReplaceInFile {
                    path: rel.clone().into(),
                    old_text: raw.to_string(),
                    new_text: raw.replace(old.as_str(), new.as_str()),
                });
                descs.push((slot, format!("  {rel}:{ln}: {}", raw.trim().replace(old.as_str(), new.as_str()))));
            }
        }
        let mut applied = Vec::new();
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
        for (slot, ops) in &by_slot {
            let slot = *slot;
            let Some(provider) = registry.entry_at(slot) else { continue };
            if let Ok(ci_core::CommitResult::Ok { changed_files, .. }) = provider.apply_edits(ops, &opts) {
                if let Err(e) = self.reindex_after_edit(&changed_files) {
                    eprintln!("[marksman-mcp] post-comment-update reindex failed: {e}");
                }
                applied.extend(descs.iter().filter(|(s, _)| *s == slot).map(|(_, d)| d.clone()));
            }
        }
        applied
    }

    /// Post-commit echo of each edited symbol's block AS COMMITTED (read back from disk):
    /// the agent verifies the edit against the real result — placement, scope, and the PROSE
    /// around it — without a read_node round-trip. When logic changes, a comment that still
    /// describes the old behavior is part of the diff; showing the whole block is what makes
    /// a now-stale comment visible in the same turn.
    fn post_edit_echo(&self, registry: &ProviderRegistry, ids: &[String]) -> Option<String> {
        if ids.is_empty() {
            return None;
        }
        let mut blocks = Vec::new();
        for id in ids.iter().take(4) {
            let file = file_of(id).to_string();
            let Ok(nodes) = registry.structure(Path::new(&file)) else { continue };
            // Gone from the tree (e.g. the node itself was deleted/renamed away): nothing to echo.
            let Some(node) = find_node(&nodes, id) else { continue };
            let Ok(content) = std::fs::read_to_string(self.root.join(&file)) else { continue };
            let text = slice_lines(&content, node.range.start_line, node.range.end_line);
            let n = text.lines().count();
            let shown = if n <= 30 {
                text
            } else {
                let head: Vec<&str> = text.lines().take(30).collect();
                format!("{}\n… ({} more lines — read_node {} if you must see them)", head.join("\n"), n - 30, id)
            };
            blocks.push(format!(
                "{id} (L{}-{}):\n```\n{shown}\n```",
                node.range.start_line, node.range.end_line
            ));
        }
        if blocks.is_empty() {
            return None;
        }
        let more = if ids.len() > 4 {
            format!("… and {} more edited symbol(s).\n", ids.len() - 4)
        } else {
            String::new()
        };
        Some(format!(
            "\npost-edit state (as committed — verify your intent against THIS, no re-read needed):\n{}\n{more}If a comment or doc line in/above the block still describes the OLD behavior, update it in your \
             next batch — when logic changes, stale prose is part of the diff.\n",
            blocks.join("\n")
        ))
    }
}

fn write_anchors(n: &Node, out: &mut String, depth: usize) {
    out.push_str(&format!(
        "{}{}  (L{}-{})\n",
        "  ".repeat(depth),
        n.id,
        n.range.start_line,
        n.range.end_line
    ));
    for c in &n.children {
        write_anchors(c, out, depth + 1);
    }
}

fn render_summary(m: &Manifest, root: &Path) -> String {
    let mut out = format!("# Context for: \"{}\"\n# {} files\n\n", m.task, m.entries.len());
    for e in &m.entries {
        out.push_str(&format!(
            "{:<16} {:.3}  {}{}\n",
            e.reason,
            e.score,
            e.file,
            if e.whole_file == Some(true) { "  (whole file)" } else { "" }
        ));
        let content = std::fs::read_to_string(root.join(&e.file)).unwrap_or_default();
        for s in &e.matched_symbols {
            // Include the node-id handle so the agent can read_node/apply_edits it directly.
            // SINGLE-LINE symbols (consts, fields, type aliases) get their source INLINE:
            // an agent that edits such a node without ever seeing it reconstructs the
            // statement from its prior — bench locate-edit: `replace_node RRF_K` hallucinated
            // `f64` for an f32 const, burning a reject round-trip the one line would prevent.
            let inline = if s.line_range[0] == s.line_range[1] {
                content
                    .lines()
                    .nth(s.line_range[0].saturating_sub(1) as usize)
                    .map(|l| {
                        let t = l.trim();
                        let t = if t.len() > 100 { &t[..100] } else { t };
                        format!("  — `{t}`")
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            out.push_str(&format!(
                "                 ↳ {} {}  L{}-{}  [{}]{inline}\n",
                s.kind.as_str(), s.name, s.line_range[0], s.line_range[1], s.node_id
            ));
        }
    }
    out
}

/// The file portion of a node id (`src/a.ts#Foo.bar:body` -> `src/a.ts`).
fn file_of(id: &str) -> &str {
    id.split('#').next().unwrap_or(id)
}

/// Word-boundary occurrences of `name` across every `.{ext}` file (gitignore-aware) — the
/// server-side verification behind an UNGATED rename's response. Textual truth on purpose:
/// comments and strings count (they're exactly what an identifier rename leaves behind), and
/// the agent gets file:line evidence instead of an instruction to go grep. Returns
/// `(repo-relative file, 1-based line, trimmed line text)`; capped, one hit per line.
fn scan_word(root: &Path, ext: &str, name: &str) -> Vec<(String, u32, String)> {
    let mut hits = Vec::new();
    let is_word = |b: Option<u8>| b.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_');
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        for (ln, line) in content.lines().enumerate() {
            let bytes = line.as_bytes();
            let mut start = 0;
            while let Some(pos) = line[start..].find(name) {
                let i = start + pos;
                let before = if i == 0 { None } else { Some(bytes[i - 1]) };
                let after = bytes.get(i + name.len()).copied();
                if !is_word(before) && !is_word(after) {
                    hits.push((rel.clone(), (ln + 1) as u32, line.trim().to_string()));
                    break; // one hit per line is enough evidence
                }
                start = i + name.len();
            }
            if hits.len() >= 8 {
                return hits;
            }
        }
    }
    hits
}

/// The symbol anchor a scan hit belongs to: the SMALLEST symbol in `rel` whose range contains
/// `line`, as `(node_id, Some("doc"))` when the line sits in that symbol's `:doc` sub-node,
/// else `(node_id, None)`. `None` = the line is outside every symbol (e.g. a file-header
/// comment) — no ready-to-copy fix can be offered, only the evidence line.
fn enclosing_anchor(registry: &ProviderRegistry, rel: &str, line: u32) -> Option<(String, Option<&'static str>)> {
    let nodes = registry.structure(Path::new(rel)).ok()?;
    let mut best: Option<(&Node, u32)> = None; // (symbol, span) — smallest containing span wins
    let mut stack: Vec<&Node> = nodes.iter().collect();
    while let Some(n) = stack.pop() {
        let contains_sym = n.range.start_line <= line && line <= n.range.end_line;
        // A `:doc` comment sits ABOVE its symbol's range — check children regardless.
        if matches!(n.kind, NodeKind::Symbol(_)) {
            let in_doc = n
                .children
                .iter()
                .any(|c| c.id.ends_with(":doc") && c.range.start_line <= line && line <= c.range.end_line);
            if contains_sym || in_doc {
                let span = n.range.end_line.saturating_sub(n.range.start_line);
                if best.map(|(_, s)| span < s).unwrap_or(true) {
                    best = Some((n, span));
                }
            }
        }
        stack.extend(n.children.iter());
    }
    let (node, _) = best?;
    let in_doc = node
        .children
        .iter()
        .any(|c| c.id.ends_with(":doc") && c.range.start_line <= line && line <= c.range.end_line);
    Some((node.id.clone(), in_doc.then_some("doc")))
}

/// The file an op edits — what decides WHICH language provider handles it (`None` only for a
/// pathological empty id; the caller then falls back to the first provider).
fn op_file(op: &ci_core::EditOp) -> Option<String> {
    use ci_core::EditOp::*;
    let f = match op {
        SetBody { node_id, .. }
        | ReplaceNode { node_id, .. }
        | ReplaceText { node_id, .. }
        | InsertBefore { node_id, .. }
        | InsertInBody { node_id, .. }
        | DeleteInBody { node_id, .. }
        | InsertMember { node_id, .. }
        | AddParameter { node_id, .. }
        | SetReturnType { node_id, .. }
        | Rename { node_id, .. } => file_of(node_id).to_string(),
        MoveFile { from, .. } => from.to_string_lossy().replace('\\', "/"),
        CreateFile { path, .. } | DeleteFile { path } | ReplaceInFile { path, .. } | AddSymbol { path, .. } => {
            path.to_string_lossy().replace('\\', "/")
        }
    };
    (!f.is_empty()).then_some(f)
}

/// No symbol's text contains the op's target: the truthful answer, instead of candidate ids
/// the op is guaranteed to fail on. File-top statements are the usual cause.
fn no_containing_symbol_msg(reference: &str) -> String {
    format!(
        "{reference:?}: no named symbol's source contains the op's target text — file-level statements (imports, `mod` declarations) sit outside every symbol anchor. rename/move ops update imports automatically; for other file-top edits use replace_text with `path` + a UNIQUE `oldText` and NO name/query — that edits the file directly, still gate-verified."
    )
}

/// Ask the agent to re-issue with one of the candidate node ids (the disambiguation reply shared
/// by name-collision and query resolution).
fn candidate_msg(reference: &str, ids: &[String]) -> String {
    format!(
        "{reference:?} is ambiguous ({} matches) — re-issue with `name` set to one of these ids:\n{}",
        ids.len(),
        ids.join("\n")
    )
}

/// Collect symbol node ids whose LEAF name matches (e.g. leaf `foo` matches `f.ts#Cls.foo`);
/// an empty `leaf` collects every named symbol id. Capped — this feeds a retry hint, not a dump.
fn collect_ids_by_leaf(nodes: &[Node], leaf: &str, out: &mut Vec<String>) {
    for n in nodes {
        if out.len() >= 20 {
            return;
        }
        if n.name.is_some() && (leaf.is_empty() || n.name.as_deref() == Some(leaf)) {
            out.push(n.id.clone());
        }
        collect_ids_by_leaf(&n.children, leaf, out);
    }
}

/// Depth-first find of a node by its anchor id (symbol or sub-node).
/// The EXACT byte extent of `r` in `content` — the text a `replace_node` value overwrites.
/// `None` when the range is line-only (both chars 0) or out of bounds; callers fall back to
/// whole lines.
fn exact_extent(content: &str, r: &ci_core::Range) -> Option<String> {
    if r.start_char == 0 && r.end_char == 0 {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let s = r.start_line.checked_sub(1)? as usize;
    let e = r.end_line.checked_sub(1)? as usize;
    if s >= lines.len() || e >= lines.len() || e < s {
        return None;
    }
    let sc = (r.start_char as usize).min(lines[s].len());
    let ec = (r.end_char as usize).min(lines[e].len());
    if s == e {
        if ec <= sc {
            return None;
        }
        return lines[s].get(sc..ec).map(str::to_string);
    }
    let mut out = lines[s].get(sc..)?.to_string();
    for l in &lines[s + 1..e] {
        out.push('\n');
        out.push_str(l);
    }
    out.push('\n');
    out.push_str(lines[e].get(..ec)?);
    Some(out)
}

/// Ops whose gate-reject needs the target's intact BEFORE to compose the retry in the same
/// response — the diagnostics show only the broken AFTER. Every node-anchored CONTENT op
/// qualifies, not just the overwriting pair: bench body-edit-rust (2026-07-05), an
/// unanchored insert_in_body landed after Rust's tail expression, the reject showed only
/// the broken tail, and the agent paid a read_node turn to find the `after` anchor the
/// original body would have handed it.
fn wants_original_extent_note(act: &str) -> bool {
    matches!(
        act,
        "replace_node"
            | "set_body"
            | "insert_in_body"
            | "delete_in_body"
            | "replace_text"
            | "insert_member"
            | "add_parameter"
            | "set_return_type"
            | "insert_before"
    )
}

/// Reject-time context for a node-anchored content op (see [`wants_original_extent_note`]):
/// the target's original text and — for the node-REPLACING pair, when its extent is narrower
/// than its lines — the exact extent the `value` overwrites. Captured while the disk is
/// still pristine and appended to gate rejections, so the agent can compose the retry
/// against the REAL boundaries instead of paying a read_node round-trip (bench
/// locate-edit-ts: `replace_node RRF_K` with a whole statement as `value` duplicated the
/// outer keywords — the reject showed only the broken AFTER, and the fix needed the BEFORE).
fn replaced_extent_note(root: &Path, registry: &ProviderRegistry, ai: usize, act: &str, id: &str) -> Option<String> {
    let file = file_of(id).to_string();
    let nodes = registry.structure(Path::new(&file)).ok()?;
    // set_body overwrites the :body sub-node when the provider exposes one.
    let node = if act == "set_body" {
        find_node(&nodes, &format!("{id}:body")).or_else(|| find_node(&nodes, id))?
    } else {
        find_node(&nodes, id)?
    };
    let content = std::fs::read_to_string(root.join(&file)).ok()?;
    let cap = |t: String| -> String {
        let n = t.lines().count();
        if n <= 12 {
            t
        } else {
            let head: Vec<&str> = t.lines().take(12).collect();
            format!("{}\n… ({} more lines — read_node {} for the rest)", head.join("\n"), n - 12, id)
        }
    };
    let lines_text = cap(slice_lines(&content, node.range.start_line, node.range.end_line));
    let mut out = format!(
        "op #{ai} ({act} {id}) targeted L{}-{}:\n```\n{lines_text}\n```",
        node.range.start_line, node.range.end_line
    );
    // The exact-extent caveat is about what `value` OVERWRITES — only true for the
    // node-replacing pair; for insert/delete/text ops the whole-node view above is the point.
    if matches!(act, "replace_node" | "set_body") {
        if let Some(ex) = exact_extent(&content, &node.range) {
            if ex.trim() != lines_text.trim() {
                out.push_str(&format!(
                    "\nits EXACT extent is `{}` — `value` replaces precisely that text; everything outside it on the line STAYS (don't repeat keywords like `export`/`const`/`pub`).",
                    cap(ex)
                ));
            }
        }
    }
    Some(out)
}

fn find_node<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
    for n in nodes {
        if n.id == id {
            return Some(n);
        }
        if let Some(f) = find_node(&n.children, id) {
            return Some(f);
        }
    }
    None
}

/// Lines `start_1..=end_1` (1-based inclusive) of `content`.
fn slice_lines(content: &str, start_1: u32, end_1: u32) -> String {
    let skip = start_1.saturating_sub(1) as usize;
    let take = (end_1.saturating_sub(start_1) + 1) as usize;
    content.lines().skip(skip).take(take).collect::<Vec<_>>().join("\n")
}

/// Skeletal outline for a file, dispatched by extension (tree-sitter, in-process).
fn outline_for(file: &str, content: &str) -> String {
    if file.ends_with(".rs") {
        lang_rust::outline(content)
    } else if file.ends_with(".ts") || file.ends_with(".tsx") || file.ends_with(".mts") || file.ends_with(".cts") {
        lang_ts::outline(content)
    } else if let Some(fb) = Path::new(file).extension().and_then(|e| e.to_str()).and_then(FbLang::from_ext) {
        lang_fallback::outline(fb, content)
    } else {
        content.to_string()
    }
}

// ── tool schemas ───────────────────────────────────────────────────────────
/// The exported surface: TWO tools — the consolidated write tool plus one mode-dispatched
/// read tool. Settled by the controlled same-day A/B (3 runs x 12 cells, 2026-07-05):
/// identical trajectories, -11.5% cost, zero native-tool leakage in 36/36 runs. The
/// six-tool surface it replaced lives on the `research/full-surface` branch (git is the
/// archive for settled experiments; the product carries no dead knobs).
const INSPECT_MODES: [&str; 5] = ["search", "symbol", "file", "node", "map"];

fn tools_list() -> Value {
    let mut tools = json!([
        {
            "name": "apply_edits",
            "description": "Apply structured code edits atomically, type-checked over the blast radius before they land — NOTHING is written unless the whole batch compiles clean, so a rejected attempt is FREE (nothing to undo, nothing corrupted). TS + Rust gated; Python structural-only (`gated:false` — verify yourself). Use this for EVERY code edit, big or SMALL; do NOT grep-then-Edit (untyped, verified by hand).\nWIDE CHANGES — the protocol for anything whose blast radius you'd otherwise hunt for (adding a REQUIRED member to a type, changing a signature): make the anchor edit ALONE, first, with no pre-reading — the rejection is the site discovery. The type-checker enumerates EVERY affected site exhaustively (searching for the sites yourself is slower and can miss some), and the reject shows each site's current source (its in-scope variables included) plus a ready-to-copy `fix:` action with the target symbol and anchor already filled in. Then re-issue ONE batch: the anchor edit + each `fix:` verbatim with only `value` filled from the shown source. Never read_node/retrieve_context/list_anchors the sites — the reject already contains their code and scope.\nADDRESSING: if the task NAMES the symbol, go STRAIGHT here — no locate step first. Use a node id (e.g. `src/http/retry.ts#parseResponse`) when you were GIVEN one (by find_symbols, list_anchors, retrieve_context, or a reject) — unique and self-locating. If you only know the FILE and the NAME, pass `name` + `path` (resolution scoped to that file); do NOT construct a `file#Name` id yourself — nested symbols' ids include their scope (`file#Class.method`), so a guessed id misses (the error then lists the file's real ids). A bare `name` alone also works (the index finds its file); a same-name collision auto-resolves when YOUR OWN edit disambiguates it (e.g. only one `timeoutMs` definition contains oldText `3000` — that one is the target); only a genuinely ambiguous name returns candidate ids to re-issue with. Don't know the name at all? pass `query` (free-text) plus `path` — your oldText resolves it when the description alone is ambiguous.\nBATCH independent edits into ONE call — they apply and type-check together, atomically. A one-line change (flip a default, fix a value) is `replace_text` BY NAME: name=`timeoutMs` oldText=`3000` newText=`5000` — no Grep, no Read, gate-verified. In gated languages (TS/Rust), `rename`/`move_file` additionally rewrite every reference/import across the repo in ONE call — a bare `move_file` is the COMPLETE move. Per language, that one action already covers: TS — %TS_MOVE%; Rust — %RUST_MOVE%. Do NOT add create_file/replace_text helpers for imports or module decls alongside it — each helper you type is output spent re-doing what the move does, and researching the sites to WRITE those helpers is the real cost. When the task states the from/to paths, send the bare move with NO exploration first: the commit response shows EVERY line the move rewrote, per file (declarations, created module files, import/`use` paths) — a pre-move survey can only re-derive that diff — and the type-check gate rejects safely if anything is off. Genuinely unsure? `dryRun:true` on the bare move returns the same verdict without writing — ONE call, cheaper than any importer survey (ungated: best-effort within the edited file — verify references yourself).\nPick the SMALLEST edit: • `replace_text` (name, oldText=substring unique within the symbol, newText) — cheapest, no read first; with NO name/query but a `path`, a UNIQUE oldText edits the FILE directly (the way to touch imports/`mod` decls/file-top lines — and config/manifest files with no compiler, e.g. a Cargo.toml or package.json line: those apply ungated, TOML/JSON syntax verified only, and land in the SAME atomic batch as code edits). • `replace_node` + target=`body`|`return`|`param.N` (0-based)|`doc`, value=new code — one sub-node. • `set_body` (name, value=new `{ … }`) — rewrite most of a body. • `insert_in_body` (name, value=statement, optional oldText=body line to insert AFTER — substring-matched and auto-indented, so never reason about whitespace; omit oldText to append at the END of the body) / `delete_in_body` (name, oldText=the line to remove). • `insert_member` (name=an interface/type/class/object symbol, value=the new member — INCLUDE its own `;` for a type/interface field or `,` for an object property) — inserted as the FIRST member of the `{ … }` block. • `add_parameter` (name, value=`x: T`) / `set_return_type` (name, value=type; to CHANGE an existing one use replace_node target:return). • `add_symbol` (path — or, when you don't know the file, `name`/`query` naming ANY symbol already in it (the server resolves the file: no locate call first); value=the complete new top-level declaration — a function, type, or test) — appends at the END of the file with spacing handled server-side; the way to ADD a symbol that doesn't exist yet (insert_before instead places code before an EXISTING symbol; if the name already exists, the reply shows its current source and points at replace_node). • `rename` (name, value=new name; path optional); `move_file` (path, value=new path); also `insert_before` / `create_file` / `delete_file`.",
            "inputSchema": {"type":"object","properties":{
                "actions":{"type":"array","description":"One or more edits, applied atomically and type-checked together — batch related edits here instead of separate calls.","items":{"type":"object","additionalProperties":false,"properties":{
                    "action":{"type":"string","enum":["rename","replace_text","replace_node","set_body","insert_in_body","delete_in_body","insert_member","add_parameter","set_return_type","insert_before","add_symbol","move_file","create_file","delete_file"],"description":"Fields per action — rename: name, value(new name) · replace_text: name, oldText, newText · replace_node: name, value(new code), target? · set_body: name, value · insert_in_body: name, value, oldText? · delete_in_body: name, oldText · insert_member: name, value · add_parameter: name, value · set_return_type: name, value · insert_before: name, value · add_symbol: path OR name/query(any symbol in the target file — the server resolves the file), value(the new top-level declaration) · move_file: path, value(new path) · create_file: path, value(source) · delete_file: path. For symbol actions `query` may replace `name`, and `path` may scope a bare `name`."},
                    "name":{"type":"string","description":"Target symbol: a node id `file#Scope.name` you were GIVEN (find_symbols/list_anchors/a reject), or a bare NAME — add `path` when you know the defining file, omit it to let the index find the file. If ambiguous, the reply lists candidate ids to re-issue with. Used by every symbol action."},
                    "query":{"type":"string","description":"Use INSTEAD of `name` when you don't know it: a free-text description of the target; the server resolves it via the index and applies if unambiguous. Pass `path` (and, for text ops, oldText) alongside — when the description alone is ambiguous, the one symbol in that file containing your oldText resolves it."},
                    "path":{"type":"string","description":"The file to resolve a bare `name` in — pass it whenever you know the defining file (avoids ambiguity; the id stays validated). Also the file path for move_file/create_file/delete_file, and optionally rename."},
                    "value":{"type":"string","description":"The new code/text for MOST actions: rename→the new name; replace_node→new node code; set_body→new `{ … }` block; insert_in_body→a statement; insert_member→the new member (include its own `;` or `,`); add_parameter→`x: T`; set_return_type→the type; add_symbol→the complete new top-level declaration; move_file→the new path; create_file→the file source. NOTE: replace_text does NOT use `value` — it uses oldText/newText."},
                    "oldText":{"type":"string","description":"replace_text: the exact substring to replace (unique within the symbol). delete_in_body: the body line to remove. insert_in_body: optional — an existing body line to insert AFTER (substring-matched, must be unique in the body); omit to append at the END of the body."},
                    "newText":{"type":"string","description":"replace_text ONLY: the replacement for `oldText`."},
                    "target":{"type":"string","pattern":"^(body|return|returnType|doc|comment|docstring|param\\.[0-9]+)$","description":"Sub-node selector for replace_node/replace_text/insert_before: `body` | `return` | `doc` | `param.N` (0-based). Anything else is rejected — never silently the whole symbol."}
                },"required":["action"],
                "allOf":[
                    {"if":{"properties":{"action":{"const":"rename"}}},"then":{"required":["name","value"]}},
                    {"if":{"properties":{"action":{"const":"replace_text"}}},"then":{"required":["oldText","newText"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"replace_node"}}},"then":{"required":["value"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"set_body"}}},"then":{"required":["value"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"insert_in_body"}}},"then":{"required":["value"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"delete_in_body"}}},"then":{"required":["oldText"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"insert_member"}}},"then":{"required":["value"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"add_parameter"}}},"then":{"required":["value"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"set_return_type"}}},"then":{"required":["value"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"insert_before"}}},"then":{"required":["value"],"anyOf":[{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"add_symbol"}}},"then":{"required":["value"],"anyOf":[{"required":["path"]},{"required":["name"]},{"required":["query"]}]}},
                    {"if":{"properties":{"action":{"const":"move_file"}}},"then":{"required":["path","value"]}},
                    {"if":{"properties":{"action":{"const":"create_file"}}},"then":{"required":["path","value"]}},
                    {"if":{"properties":{"action":{"const":"delete_file"}}},"then":{"required":["path"]}}
                ]}},
                "dryRun":{"type":"boolean","description":"Validate through the type-check gate without writing to disk."}
            },"required":["actions"]}
        },
        {
            "name": "inspect",
            "description": "Read/locate code — ONE tool, `mode`-dispatched. `search`: find code by concept/task text (query=what you need; detailLevel pointers|outline|full, pointers default). `symbol`: exact/substring NAME -> self-locating node-id handles (query=name; substring?). `file`: a file's anchors + its import/module lines (file=path). `node`: one anchor's full source (id=node id, or name [+file]). `map`: folder/architecture overview (path? scopes). Handles/ids feed apply_edits directly. To EDIT a symbol the task already NAMES, skip inspect entirely — call apply_edits by name.",
            "inputSchema": {"type":"object","properties":{
                "mode":{"type":"string","enum":["search","symbol","file","node","map"]},
                "query":{"type":"string","description":"search: the task/concept text · symbol: the name"},
                "detailLevel":{"type":"string","enum":["pointers","outline","full"],"description":"search only"},
                "substring":{"type":"boolean","description":"symbol only: match anywhere in the name"},
                "file":{"type":"string","description":"file: the path to list · node: disambiguates a name"},
                "id":{"type":"string","description":"node: a node id you were given"},
                "name":{"type":"string","description":"node: a bare symbol name"},
                "path":{"type":"string","description":"map: subtree scope"}
            },"required":["mode"]}
        }
    ]);
    // Per-language move-coverage claims come FROM the provider crates (single source of
    // truth): the sentence the agent reads about what a bare move covers lives next to the
    // code that makes it true. A future language adds its MOVE_COVERAGE constant and one
    // placeholder here — the description can never lag the capability.
    if let Some(desc) = tools[0]["description"].as_str() {
        let d = desc
            .replace("%TS_MOVE%", lang_ts::MOVE_COVERAGE)
            .replace("%RUST_MOVE%", lang_rust::MOVE_COVERAGE);
        tools[0]["description"] = json!(d);
    }
    tools
}

/// The loaded index must have been built with the model + dims the server now embeds with; a
/// mismatch means ranking is meaningless (and a differing dim would panic `cosine_normalized`).
/// Clear error → "re-run index", never a silent mis-rank or crash.
fn ensure_index_matches(meta_model: &str, meta_dims: usize, model: &str, dim: usize) -> Result<(), String> {
    if meta_model != model || meta_dims != dim {
        return Err(format!(
            "index was built with model {meta_model:?} (dim {meta_dims}) but this server uses \
             {model:?} (dim {dim}) — re-run `marksman index`"
        ));
    }
    Ok(())
}

fn resp(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn main() {
    let mut server = Server::new(resolve_root());
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    eprintln!("[marksman-mcp] ready for {}", server.root.display());
    // Build the provider + warm the TS language server in the background now, so the
    // first apply_edits is fast instead of paying a cold project load inline.
    server.start_prewarm();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg.get("id").cloned();

        let out: Option<Value> = match method {
            "initialize" => id.map(|id| {
                resp(id, json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"codeindex-rs","version":"0.1.0"}}))
            }),
            "notifications/initialized" => None,
            "ping" => id.map(|id| resp(id, json!({}))),
            "tools/list" => id.map(|id| resp(id, json!({"tools": tools_list()}))),
            "tools/call" => id.map(|id| {
                let params = &msg["params"];
                let name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];
                let result = match name {
                    "retrieve_context" => server.retrieve_context(args),
                    "describe_architecture" => server.describe_architecture(args),
                    "find_symbols" => server.find_symbols(args),
                    "list_anchors" => server.list_anchors(args),
                    "read_node" => server.read_node(args),
                    "apply_edits" => server.apply_edits(args),
                    "inspect" => server.inspect(args),
                    other => Err(format!("unknown tool: {other}")),
                };
                match result {
                    Ok(text) => resp(id, json!({"content":[{"type":"text","text":text}]})),
                    Err(e) => resp(id, json!({"content":[{"type":"text","text":e}],"isError":true})),
                }
            }),
            _ => id.map(|id| json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}})),
        };

        if let Some(out) = out {
            let _ = writeln!(stdout, "{out}");
            let _ = stdout.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ensure_index_matches, scan_word, tools_list};

    // The BEFORE-context contract, tied to the schema so a new action can't silently opt out:
    // every node-anchored content action gets the original-extent note on a gate reject (the
    // diagnostics show only the broken AFTER); rename has its own scan, add_symbol's conflict
    // reply already echoes the existing source, and file ops have no node to echo.
    #[test]
    fn every_content_action_gets_the_original_extent_note() {
        let tools = tools_list();
        let actions: Vec<String> = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "apply_edits")
            .unwrap()["inputSchema"]["properties"]["actions"]["items"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        for act in &actions {
            let exempt = matches!(act.as_str(), "rename" | "add_symbol" | "move_file" | "create_file" | "delete_file");
            assert_eq!(
                super::wants_original_extent_note(act),
                !exempt,
                "{act}: original-extent note coverage must match the exemption list"
            );
        }
    }

    // The note itself, through a real provider: a body op's reject context is the WHOLE
    // original node (that's where the retry's `after` anchor lives — bench body-edit-rust),
    // and the exact-extent overwrite caveat stays scoped to the node-replacing pair.
    #[test]
    fn original_extent_note_carries_the_whole_node_for_body_ops() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("m.py"),
            "def lookup(store, key):\n    rows = store.rows(key)\n    return rows[0]\n",
        )
        .unwrap();
        let registry = ci_build::ProviderRegistry::single(std::sync::Arc::new(
            lang_fallback::FallbackProvider::new(root, lang_fallback::FbLang::Python),
        ));
        let note = super::replaced_extent_note(root, &registry, 0, "insert_in_body", "m.py#lookup")
            .expect("note for a resolvable node");
        assert!(
            note.contains("rows = store.rows(key)"),
            "the original body (the `after` anchor source) is in the note: {note}"
        );
        assert!(
            !note.contains("EXACT extent"),
            "the overwrite caveat must not fire for insert ops: {note}"
        );
    }

    // The narrow config/manifest surface: file-level replace_text on non-provider text files.
    // TOML/JSON must still PARSE post-edit (the honest syntax gate — never a type-check),
    // oldText must be unique, the receipt carries the −/+ lines, and write:false (the gate
    // pass) stages nothing on disk.
    #[test]
    fn text_ops_syntax_gate_and_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"corpus-old\"\nversion = \"0.1.0\"\n").unwrap();
        let srv = super::Server::new(root.clone());
        let op = |old: &str, new: &str| ci_core::EditOp::ReplaceInFile {
            path: "Cargo.toml".into(),
            old_text: old.into(),
            new_text: new.into(),
        };

        // Gate pass (write:false): validates, reports, writes NOTHING.
        let (files, receipt) =
            srv.apply_text_ops(&[op("name = \"corpus-old\"", "name = \"corpus-new\"")], false).unwrap();
        assert_eq!(files, vec!["Cargo.toml".to_string()]);
        assert!(
            receipt.contains("- name = \"corpus-old\"") && receipt.contains("+ name = \"corpus-new\""),
            "receipt carries the −/+ lines: {receipt}"
        );
        assert!(
            std::fs::read_to_string(root.join("Cargo.toml")).unwrap().contains("corpus-old"),
            "the gate pass must not write"
        );

        // An edit that breaks TOML syntax rejects.
        let err = srv.apply_text_ops(&[op("[package]", "[package")], false).unwrap_err();
        assert!(err.contains("no longer parse as TOML"), "syntax gate holds: {err}");

        // Non-unique oldText rejects with the extend-it guidance.
        let err = srv.apply_text_ops(&[op("\"", "'")], false).unwrap_err();
        assert!(err.contains("extend it until unique"), "{err}");

        // write:true commits.
        srv.apply_text_ops(&[op("name = \"corpus-old\"", "name = \"corpus-new\"")], true).unwrap();
        assert!(std::fs::read_to_string(root.join("Cargo.toml")).unwrap().contains("corpus-new"));

        // Re-issuing the same edit is SATISFIED (newText already present), not an error.
        let (files, receipt) =
            srv.apply_text_ops(&[op("name = \"corpus-old\"", "name = \"corpus-new\"")], true).unwrap();
        assert!(files.is_empty(), "nothing changed the second time");
        assert!(receipt.contains("already satisfied"), "{receipt}");
    }

    // The post-rename scan is word-boundary and per-extension: `rollup_day` must not match
    // `daily_rollup_day2` or a `.go` file, and a comment mention IS a hit (that's the point —
    // it's exactly what an identifier rename leaves behind for the agent to decide on).
    #[test]
    fn scan_word_is_word_boundary_and_ext_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.py"), "# rollup_day is folded here\nx = rollup_day2()\nrollup_day()\n").unwrap();
        std::fs::write(root.join("b.go"), "// rollup_day mentioned in go\n").unwrap();
        let hits = scan_word(root, "py", "rollup_day");
        let lines: Vec<u32> = hits.iter().map(|(_, l, _)| *l).collect();
        assert_eq!(lines, vec![1, 3], "comment + call hit; rollup_day2 and .go excluded: {hits:?}");
        assert!(hits.iter().all(|(f, _, _)| f == "a.py"));
        assert!(scan_word(root, "py", "absent_name").is_empty());
    }

    // The schema's `action` enum and the edit engine's dispatch are maintained separately; this
    // pins them together — every action the schema advertises must be one action_to_op accepts
    // (an enum entry the engine rejects would send the agent into "unsupported action" loops).
    #[test]
    fn schema_action_enum_matches_the_edit_engine() {
        let tools = tools_list();
        let actions = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "apply_edits")
            .unwrap()["inputSchema"]["properties"]["actions"]["items"]["properties"]["action"]["enum"]
            .as_array()
            .expect("apply_edits action enum")
            .clone();
        assert!(actions.len() >= 13, "action enum unexpectedly small: {actions:?}");
        let resolve = |_: &str, _: Option<&str>, _: Option<&str>| Some("f.ts#x".to_string());
        for a in actions {
            let act = a.as_str().unwrap();
            let action = ci_edit::Action {
                path: "f.ts".into(),
                action: act.into(),
                target: None,
                name: Some("x".into()),
                value: Some("v".into()),
                old_text: Some("o".into()),
                new_text: Some("n".into()),
            };
            if let Err(e) = ci_edit::action_to_op(&action, resolve) {
                assert!(
                    !e.to_string().contains("unsupported action"),
                    "schema advertises {act:?} but the edit engine rejects it: {e}"
                );
            }
        }
    }

    // The reject-time BEFORE-text hinges on exact_extent slicing the node's true byte span:
    // a declarator inside `export const X = 60;` is `X = 60`, NOT the whole line — showing
    // the line as "the extent" would re-teach the exact mis-scope that caused the reject.
    #[test]
    fn exact_extent_slices_true_byte_spans() {
        use ci_core::Range;
        let content = "export const RRF_K = 60;\nfn f() {\n  body();\n}\n";
        // sub-line span: the declarator only.
        let r = Range { start_line: 1, end_line: 1, start_char: 13, end_char: 23 };
        assert_eq!(super::exact_extent(content, &r).as_deref(), Some("RRF_K = 60"));
        // multi-line span with byte cols.
        let r = Range { start_line: 2, end_line: 4, start_char: 7, end_char: 1 };
        assert_eq!(super::exact_extent(content, &r).as_deref(), Some("{\n  body();\n}"));
        // line-only drivers (both chars 0) opt out — callers show whole lines instead.
        let r = Range { start_line: 1, end_line: 1, start_char: 0, end_char: 0 };
        assert_eq!(super::exact_extent(content, &r), None);
        // out-of-bounds never panics.
        let r = Range { start_line: 9, end_line: 9, start_char: 1, end_char: 2 };
        assert_eq!(super::exact_extent(content, &r), None);
    }

    // The surface is TWO tools whose inspect schema modes exactly match the dispatcher's
    // arms — a schema mode the dispatcher rejects would send agents into unknown-mode loops.
    #[test]
    fn surface_is_two_tools_with_matching_modes() {
        let tools = super::tools_list();
        let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["apply_edits", "inspect"]);
        let schema_modes: Vec<&str> = tools[1]["inputSchema"]["properties"]["mode"]["enum"]
            .as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
        assert_eq!(schema_modes, super::INSPECT_MODES.to_vec(), "schema enum == dispatcher modes");
    }

    // The move-coverage sentences the agent reads are provider-owned constants; this pins the
    // placeholder wiring so a refactor can't silently ship "%TS_MOVE%" as literal prose (or
    // sever the claim from the crate that implements it).
    #[test]
    fn move_coverage_claims_come_from_the_provider_crates() {
        let tools = tools_list();
        let desc = tools[0]["description"].as_str().unwrap();
        assert_eq!(tools[0]["name"], "apply_edits", "apply_edits leads the listing");
        assert!(desc.contains(lang_ts::MOVE_COVERAGE), "ts claim wired in");
        assert!(desc.contains(lang_rust::MOVE_COVERAGE), "rust claim wired in");
        assert!(!desc.contains("%TS_MOVE%") && !desc.contains("%RUST_MOVE%"), "no unexpanded placeholders");
    }

    #[test]
    fn index_compat_guard() {
        assert!(ensure_index_matches("potion", 256, "potion", 256).is_ok());
        // dim mismatch (the cosine-panic guard) and model mismatch both error clearly.
        let dim_err = ensure_index_matches("potion", 256, "potion", 128).unwrap_err();
        assert!(dim_err.contains("re-run"), "dim mismatch: {dim_err}");
        let model_err = ensure_index_matches("bge", 256, "potion", 256).unwrap_err();
        assert!(model_err.contains("re-run"), "model mismatch: {model_err}");
    }
}
