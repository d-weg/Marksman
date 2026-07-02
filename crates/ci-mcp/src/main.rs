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
            eprintln!("[marksman-mcp] language: rust (tree-sitter, in-process — no Node)");
            ProviderBuild::Ready(Arc::new(RustProvider::new(root).with_scip(config.scip_enabled("rust"))))
        }
        "ts" => {
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
        let mut out = render_summary(&manifest);
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
        let nodes = self.registry()?.structure(Path::new(&file)).map_err(|e| e.to_string())?;
        let mut out = String::new();
        for n in &nodes {
            write_anchors(n, &mut out, 0);
        }
        Ok(if out.is_empty() { "(no symbols)".into() } else { out })
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
        if !path.is_empty() {
            let nodes = registry.structure(Path::new(path)).unwrap_or_default();
            let ids = resolve_all_in(&nodes, reference);
            if !ids.is_empty() {
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
        if !named.is_empty() {
            let ids: Vec<String> = named.iter().filter_map(|(f, n)| id_in(f, n)).collect();
            return match ids.len() {
                1 => Ok(ids.into_iter().next().unwrap()),
                _ => self
                    .resolve_by_containment(registry, path, op_needle)
                    .ok_or_else(|| candidate_msg(query, &ids)),
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
        match ids.len() {
            1 => Ok(ids.into_iter().next().unwrap()),
            _ => self.resolve_by_containment(registry, path, op_needle).ok_or_else(|| {
                if ids.is_empty() {
                    format!("query {query:?} resolved to no symbol — use retrieve_context to find it, then edit by name/id")
                } else {
                    candidate_msg(query, &ids)
                }
            }),
        }
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

    fn apply_edits(&mut self, args: &Value) -> Result<String, String> {
        let dry_run = args["dryRun"].as_bool().unwrap_or(false);
        let actions = args["actions"].as_array().ok_or("`actions` array is required")?.clone();
        let registry = self.registry()?;

        let mut ops = Vec::new();
        for (ai, a) in actions.iter().enumerate() {
            // Reject unknown fields UP FRONT: a misspelled field (`old_text`, `after`) would
            // otherwise be silently dropped — and for insert_in_body a dropped `after` doesn't
            // error, it silently changes WHERE the code lands (end of body). A clear one-round-trip
            // correction beats a wrong edit that type-checks.
            if let Some(obj) = a.as_object() {
                const KNOWN: [&str; 8] = ["action", "name", "query", "path", "value", "oldText", "newText", "target"];
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
            let path = a["path"].as_str().unwrap_or("").to_string();
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
            if name.is_none() {
                if let Some(q) = a["query"].as_str() {
                    name = Some(self.resolve_query(&registry, q, &path, op_needle)?);
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
            if symbol_action {
                if let Some(reference) = name.as_deref() {
                    name = Some(self.resolve_symbol(&registry, &path, reference, op_needle)?);
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
        let mut groups: Vec<(usize, Vec<ci_core::EditOp>)> = Vec::new();
        for op in ops {
            let slot = match op_file(&op) {
                Some(f) => registry.entry_for(Path::new(&f)).ok_or_else(|| {
                    // The dependency layer: when the language is present but its toolchain is
                    // missing, hand the agent/user the exact install instruction — not a shrug.
                    match registry.disabled_reason(Path::new(&f)) {
                        Some(reason) => format!("no provider for '{f}' — its language is disabled on this machine:\n{reason}"),
                        None => format!("no language provider for '{f}' — its language isn't active in this repo (is its toolchain available?)"),
                    }
                })?,
                None => 0, // path-less op: first provider, as before
            };
            match groups.iter_mut().find(|(s, _)| *s == slot) {
                Some((_, v)) => v.push(op),
                None => groups.push((slot, vec![op])),
            }
        }
        if groups.is_empty() {
            return Ok("Applied 0 edit(s); no file changes were necessary.".into());
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
                    for (gi, (slot, gops)) in groups.iter().enumerate() {
                        match provider_of(*slot).apply_edits(gops, &opts).map_err(|e| e.to_string())? {
                            ci_core::CommitResult::Ok { applied_ops, changed_files, .. } => {
                                applied += applied_ops;
                                changed.extend(changed_files);
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
                    ci_core::CommitResult::Ok { applied_ops: applied, changed_files: changed, repair_rounds: 0 }
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
        match res {
            ci_core::CommitResult::Ok { applied_ops, changed_files, .. } if changed_files.is_empty() => {
                Ok(format!(
                    "Applied {applied_ops} edit(s){}; no file changes were necessary.",
                    if dry_run { " (dry run)" } else { "" }
                ))
            }
            ci_core::CommitResult::Ok { applied_ops, changed_files, .. } if all_gated => Ok(format!(
                "✓ Applied {applied_ops} edit(s){}; {} file(s) changed; type-checked clean — no new type errors anywhere, \
                 including files that import what changed. rename/move already updated every reference/import across the \
                 whole codebase, so this change is COMPLETE — do not grep, re-read, or hand-edit call sites to verify.\nFiles changed:\n{}",
                if dry_run { " (dry run — nothing written yet)" } else { "" },
                changed_files.len(),
                changed_files.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"),
            )),
            // Ungated (tree-sitter fallback — or a mixed batch touching one such language):
            // structural edit, NOT type-checked. Be honest so the agent knows to verify — and
            // that rename was best-effort within the edited file only.
            ci_core::CommitResult::Ok { applied_ops, changed_files, .. } => Ok(format!(
                "✓ Applied {applied_ops} structural edit(s){}; {} file(s) changed. gated: false — at least one edited \
                 language has no type-checker wired up, so those edits were NOT verified to compile, and their `rename` \
                 rewrote matching identifiers within the edited file only (not cross-file references). Review or run the \
                 project's own checks to confirm.\nFiles changed:\n{}",
                if dry_run { " (dry run — nothing written yet)" } else { "" },
                changed_files.len(),
                changed_files.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"),
            )),
            ci_core::CommitResult::Rejected { feedback, .. } => {
                Err(format!("rejected — nothing written:\n{feedback}"))
            }
        }
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

fn render_summary(m: &Manifest) -> String {
    let mut out = format!("# Context for: \"{}\"\n# {} files\n\n", m.task, m.entries.len());
    for e in &m.entries {
        out.push_str(&format!(
            "{:<16} {:.3}  {}{}\n",
            e.reason,
            e.score,
            e.file,
            if e.whole_file == Some(true) { "  (whole file)" } else { "" }
        ));
        for s in &e.matched_symbols {
            // Include the node-id handle so the agent can read_node/apply_edits it directly.
            out.push_str(&format!("                 ↳ {} {}  L{}-{}  [{}]\n", s.kind.as_str(), s.name, s.line_range[0], s.line_range[1], s.node_id));
        }
    }
    out
}

/// The file portion of a node id (`src/a.ts#Foo.bar:body` -> `src/a.ts`).
fn file_of(id: &str) -> &str {
    id.split('#').next().unwrap_or(id)
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
        CreateFile { path, .. } | DeleteFile { path } => path.to_string_lossy().replace('\\', "/"),
    };
    (!f.is_empty()).then_some(f)
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
fn tools_list() -> Value {
    json!([
        {
            "name": "retrieve_context",
            "description": "Find files + line-ranges relevant to a task (hybrid BM25 + Model2Vec + symbol match, RRF-fused, expanded along the import graph; no API calls). `detailLevel`: `pointers` (default — file+line pointers only, by far the cheapest; use it to LOCATE code you'll then edit or read_node), `outline` (files inlined with function/method BODIES elided — exact signatures/args/return types, not bodies; a 200-line file → ~15 lines), `full` (whole files; import-graph neighbors stay outline). Use outline/full only when you must read several files' code at once, not merely find them. To EDIT a named symbol you don't need this at all — call apply_edits by name directly.",
            "inputSchema": {"type":"object","properties":{"task":{"type":"string"},"topN":{"type":"integer"},"hops":{"type":"integer"},"detailLevel":{"type":"string","enum":["pointers","outline","full"]}},"required":["task"]}
        },
        {
            "name": "describe_architecture",
            "description": "Folder/architecture map (zero-API): per-directory file-kind patterns and detected module templates. Optional `path` scopes to a subtree.",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string"}}}
        },
        {
            "name": "find_symbols",
            "description": "Exact/substring search over indexed symbol NAMES → self-locating node-id handles (with kind + line range), NOT file:line. The cheap bridge from a known name to an editable handle: results feed straight into read_node (id=…, incl. …:body/:doc) or apply_edits (name=… / the id). Prefer over retrieve_context when you know the name, and over grep when you'll act on the symbol next. Exhaustive (good for audits — every symbol named/containing X), not top-k. `substring:true` matches anywhere in the name; omit for whole-name.",
            "inputSchema": {"type":"object","properties":{
                "query":{"type":"string","description":"The symbol name to search for."},
                "substring":{"type":"boolean","description":"Match anywhere in the name (default false = whole-name match)."}
            },"required":["query"]}
        },
        {
            "name": "list_anchors",
            "description": "List AST anchors (node ids + line ranges) in a file — symbols and their sub-nodes (params/return/body) — to target with apply_edits or read_node.",
            "inputSchema": {"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}
        },
        {
            "name": "read_node",
            "description": "Full source + metadata of ONE anchor (a symbol, or its :body / :param.N / :return / :doc sub-node) — the drill-down after an `outline` elided a body. Address by `id` (a node id, e.g. 'src/http/retry.ts#RetryPolicy.execute' or '…#execute:body' — self-locating, no `file`) or by `name` (file found via the index; pass `file` only to disambiguate).",
            "inputSchema": {"type":"object","properties":{
                "id":{"type":"string","description":"A node id, e.g. 'src/bm25.ts#BM25.search' or a sub-node '…#search:body' — self-locating, needs no `file`. Use ids you were GIVEN; a constructed one may miss a nested symbol's scope (the error lists the file's real ids)."},
                "name":{"type":"string","description":"A bare symbol name; its file is found via the index. Pass `file` only to disambiguate."},
                "file":{"type":"string","description":"Optional: repo-relative file to disambiguate a `name` defined in more than one file."}
            },"anyOf":[{"required":["id"]},{"required":["name"]}]}
        },
        {
            "name": "apply_edits",
            "description": "Apply structured code edits atomically, type-checked over the blast radius before they land — NOTHING is written unless the whole batch compiles clean, so a rejected attempt is FREE (nothing to undo, nothing corrupted). TS + Rust gated; Python structural-only (`gated:false` — verify yourself). Use this for EVERY code edit, big or SMALL; do NOT grep-then-Edit (untyped, verified by hand).\nWIDE CHANGES — the protocol for anything whose blast radius you'd otherwise hunt for (adding a REQUIRED member to a type, changing a signature): make the anchor edit ALONE, first, with no pre-reading — the rejection is the site discovery. The type-checker enumerates EVERY affected site exhaustively (searching for the sites yourself is slower and can miss some), and the reject shows each site's current source (its in-scope variables included) plus a ready-to-copy `fix:` action with the target symbol and anchor already filled in. Then re-issue ONE batch: the anchor edit + each `fix:` verbatim with only `value` filled from the shown source. Never read_node/retrieve_context/list_anchors the sites — the reject already contains their code and scope.\nADDRESSING: if the task NAMES the symbol, go STRAIGHT here — no locate step first. Use a node id (e.g. `src/http/retry.ts#parseResponse`) when you were GIVEN one (by find_symbols, list_anchors, retrieve_context, or a reject) — unique and self-locating. If you only know the FILE and the NAME, pass `name` + `path` (resolution scoped to that file); do NOT construct a `file#Name` id yourself — nested symbols' ids include their scope (`file#Class.method`), so a guessed id misses (the error then lists the file's real ids). A bare `name` alone also works (the index finds its file); a same-name collision auto-resolves when YOUR OWN edit disambiguates it (e.g. only one `timeoutMs` definition contains oldText `3000` — that one is the target); only a genuinely ambiguous name returns candidate ids to re-issue with. Don't know the name at all? pass `query` (free-text) plus `path` — your oldText resolves it when the description alone is ambiguous.\nBATCH independent edits into ONE call — they apply and type-check together, atomically. A one-line change (flip a default, fix a value) is `replace_text` BY NAME: name=`timeoutMs` oldText=`3000` newText=`5000` — no Grep, no Read, gate-verified. In gated languages (TS/Rust), `rename`/`move_file` additionally rewrite every reference/import across the repo in ONE call (ungated: best-effort within the edited file — verify references yourself).\nPick the SMALLEST edit: • `replace_text` (name, oldText=substring unique within the symbol, newText) — cheapest, no read first. • `replace_node` + target=`body`|`return`|`param.N` (0-based)|`doc`, value=new code — one sub-node. • `set_body` (name, value=new `{ … }`) — rewrite most of a body. • `insert_in_body` (name, value=statement, optional oldText=body line to insert AFTER — substring-matched and auto-indented, so never reason about whitespace; omit oldText to append at the END of the body) / `delete_in_body` (name, oldText=the line to remove). • `insert_member` (name=an interface/type/class/object symbol, value=the new member — INCLUDE its own `;` for a type/interface field or `,` for an object property) — inserted as the FIRST member of the `{ … }` block. • `add_parameter` (name, value=`x: T`) / `set_return_type` (name, value=type; to CHANGE an existing one use replace_node target:return). • `rename` (name, value=new name; path optional); `move_file` (path, value=new path); also `insert_before` / `create_file` / `delete_file`.",
            "inputSchema": {"type":"object","properties":{
                "actions":{"type":"array","description":"One or more edits, applied atomically and type-checked together — batch related edits here instead of separate calls.","items":{"type":"object","additionalProperties":false,"properties":{
                    "action":{"type":"string","enum":["rename","replace_text","replace_node","set_body","insert_in_body","delete_in_body","insert_member","add_parameter","set_return_type","insert_before","move_file","create_file","delete_file"],"description":"Fields per action — rename: name, value(new name) · replace_text: name, oldText, newText · replace_node: name, value(new code), target? · set_body: name, value · insert_in_body: name, value, oldText? · delete_in_body: name, oldText · insert_member: name, value · add_parameter: name, value · set_return_type: name, value · insert_before: name, value · move_file: path, value(new path) · create_file: path, value(source) · delete_file: path. For symbol actions `query` may replace `name`, and `path` may scope a bare `name`."},
                    "name":{"type":"string","description":"Target symbol: a node id `file#Scope.name` you were GIVEN (find_symbols/list_anchors/a reject), or a bare NAME — add `path` when you know the defining file, omit it to let the index find the file. If ambiguous, the reply lists candidate ids to re-issue with. Used by every symbol action."},
                    "query":{"type":"string","description":"Use INSTEAD of `name` when you don't know it: a free-text description of the target; the server resolves it via the index and applies if unambiguous. Pass `path` (and, for text ops, oldText) alongside — when the description alone is ambiguous, the one symbol in that file containing your oldText resolves it."},
                    "path":{"type":"string","description":"The file to resolve a bare `name` in — pass it whenever you know the defining file (avoids ambiguity; the id stays validated). Also the file path for move_file/create_file/delete_file, and optionally rename."},
                    "value":{"type":"string","description":"The new code/text for MOST actions: rename→the new name; replace_node→new node code; set_body→new `{ … }` block; insert_in_body→a statement; insert_member→the new member (include its own `;` or `,`); add_parameter→`x: T`; set_return_type→the type; move_file→the new path; create_file→the file source. NOTE: replace_text does NOT use `value` — it uses oldText/newText."},
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
                    {"if":{"properties":{"action":{"const":"move_file"}}},"then":{"required":["path","value"]}},
                    {"if":{"properties":{"action":{"const":"create_file"}}},"then":{"required":["path","value"]}},
                    {"if":{"properties":{"action":{"const":"delete_file"}}},"then":{"required":["path"]}}
                ]}},
                "dryRun":{"type":"boolean","description":"Validate through the type-check gate without writing to disk."}
            },"required":["actions"]}
        }
    ])
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
    use super::{ensure_index_matches, tools_list};

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
