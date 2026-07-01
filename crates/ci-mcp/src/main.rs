//! codeindex-rs MCP server (stdio, JSON-RPC 2.0, newline-delimited). Exposes the
//! input tools (retrieve_context, describe_architecture, find_symbols) and the
//! output tools (list_anchors, read_node, apply_edits). Launch per repo:
//!   codeindex-rs-mcp --root /path/to/repo   (or $CODEINDEX_ROOT, or cwd)
//!
//! The server is pure-Rust orchestration; all language/external tooling is behind
//! the `lang-ts` provider.
use ci_arch::{build_architecture, format_architecture};
use ci_build::{build_registry, ProviderRegistry};
use ci_core::{Config, EditOpts, LanguageProvider, Manifest, Node, NodeKind};
use ci_edit::{action_to_op, resolve_in, Action};
use ci_embed::StaticEmbedder;
use ci_index::{index_exists, load_index, save_index};
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
/// `CI_PROVIDER=sidecar`. `None` (with a warning) when a language's tooling can't start, so one
/// language failing doesn't sink a mixed-language repo. Called once per active language by
/// [`build_registry`], so Node's `scip-typescript` only runs when the repo has `.ts*` files.
fn make_provider(lang: &str, root: &Path, config: &Config) -> Option<Arc<dyn LanguageProvider>> {
    if std::env::var("CI_PROVIDER").as_deref() == Ok("sidecar") {
        if let Some(cmd) = ci_proto::sidecar_command_with(lang, root, false, config.provider_bin(lang)) {
            eprintln!("[codeindex-rs-mcp] language: {lang} (sidecar process — protobuf wire)");
            match ProcessProvider::spawn(cmd) {
                Ok(p) => return Some(Arc::new(p)),
                Err(e) => {
                    eprintln!("[codeindex-rs-mcp] sidecar {lang} failed to start ({e}); skipping");
                    return None;
                }
            }
        }
        eprintln!("[codeindex-rs-mcp] CI_PROVIDER=sidecar but no marksman-provider-{lang} found — using in-process");
    }
    match lang {
        "rust" => {
            eprintln!("[codeindex-rs-mcp] language: rust (tree-sitter, in-process — no Node)");
            Some(Arc::new(RustProvider::new(root).with_scip(config.scip_enabled("rust"))))
        }
        "python" => {
            eprintln!("[codeindex-rs-mcp] language: python (tree-sitter fallback, in-process — edits are ungated)");
            Some(Arc::new(FallbackProvider::new(root, FbLang::Python)))
        }
        "ts" => {
            eprintln!("[codeindex-rs-mcp] language: typescript — running scip-typescript on {} …", root.display());
            match TsProvider::index(root) {
                Ok(p) => Some(Arc::new(p)),
                Err(e) => {
                    eprintln!("[codeindex-rs-mcp] typescript indexing failed ({e}); skipping TS files");
                    None
                }
            }
        }
        _ => None,
    }
}

/// The extension → provider registry for `root`, dispatching each file to its language's provider
/// so a mixed repo reads/edits fully. Absent/disabled languages register nothing.
fn build_registry_for(root: &Path) -> Result<ProviderRegistry, String> {
    let mut config = Config::load(root).unwrap_or_default();
    config.index_dir = ".codeindex-rs".into();
    let cfg = config.clone();
    build_registry(root, &mut config, |lang| make_provider(lang, root, &cfg)).map_err(|e| e.to_string())
}

fn resolve_root() -> PathBuf {
    let argv: Vec<String> = std::env::args().collect();
    if let Some(i) = argv.iter().position(|a| a == "--root") {
        if let Some(p) = argv.get(i + 1) {
            return PathBuf::from(p);
        }
    }
    std::env::var("CODEINDEX_ROOT")
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
}

impl Server {
    fn new(root: PathBuf) -> Self {
        let mut config = Config::load(&root).unwrap_or_default();
        config.embedding_model = "minishlab/potion-code-16M".into();
        config.index_dir = ".codeindex-rs".into();
        Server { root, config, registry: Arc::new(Mutex::new(None)), embedder: None }
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
            return Err("no index — run `codeindex-rs index <root>` first".into());
        }
        let index = load_index(&self.root, &self.config).map_err(|e| e.to_string())?;
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
                if shown >= 4 || e.reason == "doc" || e.file.ends_with(".md") {
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
            return Err("no index — run `codeindex-rs index` first".into());
        }
        let index = load_index(&self.root, &self.config).map_err(|e| e.to_string())?;
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
    fn resolve_symbol(&self, registry: &ProviderRegistry, path: &str, reference: &str) -> Result<String, String> {
        if reference.contains('#') {
            return Ok(reference.to_string()); // already a node_id
        }
        if !path.is_empty() {
            let nodes = registry.structure(Path::new(path)).unwrap_or_default();
            if let Some(id) = resolve_in(&nodes, reference) {
                return Ok(id);
            }
        }
        let files = self.files_defining(reference)?;
        let id_in = |f: &str| resolve_in(&registry.structure(Path::new(f)).unwrap_or_default(), reference);
        let ids: Vec<String> = files.iter().filter_map(|f| id_in(f)).collect();
        match ids.len() {
            0 => Err(format!("symbol '{reference}' not found in the index — pass a `path`, or a node id from list_anchors/retrieve_context")),
            1 => Ok(ids.into_iter().next().unwrap()),
            _ => Err(format!(
                "'{reference}' is ambiguous ({} definitions). Re-issue with one of these as `name`:\n{}",
                ids.len(),
                ids.join("\n")
            )),
        }
    }

    /// Resolve a free-text `query` to a single node_id (the fuzziest addressing mode — fuse
    /// locate+edit into one call). Conservative + gated: an exact symbol-NAME token in the query
    /// resolves directly when unique; otherwise it falls back to retrieval and **only** auto-
    /// resolves when the top result is unambiguous — any ambiguity returns candidate ids for the
    /// agent to pick, so we never silently edit a vague guess.
    fn resolve_query(&mut self, registry: &ProviderRegistry, query: &str) -> Result<String, String> {
        let id_in = |f: &str, n: &str| resolve_in(&registry.structure(Path::new(f)).unwrap_or_default(), n);
        if !index_exists(&self.root, &self.config) {
            return Err("no index — run `codeindex-rs index` first, or address by name/id".into());
        }
        // 1) an exact symbol name that appears as a token in the query.
        let index = load_index(&self.root, &self.config).map_err(|e| e.to_string())?;
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
                _ => Err(candidate_msg(query, &ids)),
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
            0 => Err(format!("query {query:?} resolved to no symbol — use retrieve_context to find it, then edit by name/id")),
            1 => Ok(ids.into_iter().next().unwrap()),
            _ => Err(candidate_msg(query, &ids)),
        }
    }

    /// Repo-relative files that define a symbol with this exact (bare) name, from the index.
    fn files_defining(&self, name: &str) -> Result<Vec<String>, String> {
        if !index_exists(&self.root, &self.config) {
            return Ok(vec![]);
        }
        let index = load_index(&self.root, &self.config).map_err(|e| e.to_string())?;
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
            self.resolve_symbol(&registry, args["file"].as_str().unwrap_or(""), name)?
        } else {
            return Err("provide `id` (a node id from list_anchors) or `name`".into());
        };
        let file = file_of(&id).to_string();
        let nodes = registry.structure(Path::new(&file)).map_err(|e| e.to_string())?;
        let node = find_node(&nodes, &id).ok_or_else(|| format!("anchor '{id}' not found in {file}"))?;
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

        // Which file this batch edits, so we dispatch to the right language's edit engine. Set from
        // the first action's path, else the file of the first resolved node_id (`file#…`).
        let mut target_file: Option<String> = None;
        let mut note_target = |f: &str| {
            if target_file.is_none() && !f.is_empty() {
                target_file = Some(f.to_string());
            }
        };

        let mut ops = Vec::new();
        for a in &actions {
            let act = a["action"].as_str().unwrap_or("");
            let path = a["path"].as_str().unwrap_or("").to_string();
            note_target(&path);
            let mut name = a["name"].as_str().map(str::to_string);
            // `query` — the fuzziest target: resolve a free-text description to a node_id via the
            // index/retrieval (fuse locate+edit). Only when no explicit name/id was given.
            if name.is_none() {
                if let Some(q) = a["query"].as_str() {
                    name = Some(self.resolve_query(&registry, q)?);
                }
            }
            // For a symbol-targeting action, resolve the reference to a node_id UP FRONT through
            // the addressing model (id ≫ name-in-file ≫ name-in-index). This is what lets the
            // agent edit by name with no prior retrieve — the index supplies the file — and turns
            // a same-name collision into a candidate list instead of an error. File ops
            // (move/create/delete) carry no symbol, so they're left untouched.
            let symbol_action = matches!(
                act,
                "rename" | "replace" | "replace_node" | "replace_text" | "set_body" | "insert_before"
                    | "insert_in_body" | "delete_in_body" | "add_parameter" | "set_return_type"
            );
            if symbol_action {
                if let Some(reference) = name.as_deref() {
                    name = Some(self.resolve_symbol(&registry, &path, reference)?);
                }
            }
            // A resolved node_id (`file#…`) also pins the target file.
            if let Some(n) = name.as_deref() {
                if n.contains('#') {
                    note_target(file_of(n));
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

        // Dispatch to the edit engine for the target file's language (fall back to any registered
        // provider for an empty batch). A cross-language batch isn't a real scenario — the gate is
        // per-language — so one engine handles the batch.
        let provider = target_file
            .as_deref()
            .and_then(|f| registry.provider_for(Path::new(f)))
            .or_else(|| registry.providers().next())
            .ok_or("no language provider available for this repo")?;

        let opts = EditOpts { write: !dry_run, dry_run, tsconfig: None };
        let res = provider.apply_edits(&ops, &opts).map_err(|e| e.to_string())?;
        // Keep the index true: after a real (written) commit, incrementally reindex the changed
        // files so the same session's next retrieve_context/list_anchors sees the new state. A
        // reindex hiccup must NOT fail the (already-committed) edit — log and carry on stale.
        if let ci_core::CommitResult::Ok { changed_files, .. } = &res {
            if !dry_run && !changed_files.is_empty() {
                if let Err(e) = self.reindex_after_edit(changed_files) {
                    eprintln!("[codeindex-rs-mcp] post-edit reindex failed (index may be stale until next `index`): {e}");
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
            ci_core::CommitResult::Ok { applied_ops, changed_files, .. } if provider.gated() => Ok(format!(
                "✓ Applied {applied_ops} edit(s){}; {} file(s) changed; type-checked clean — no new type errors anywhere, \
                 including files that import what changed. rename/move already updated every reference/import across the \
                 whole codebase, so this change is COMPLETE — do not grep, re-read, or hand-edit call sites to verify.\nFiles changed:\n{}",
                if dry_run { " (dry run — nothing written yet)" } else { "" },
                changed_files.len(),
                changed_files.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"),
            )),
            // Ungated (tree-sitter fallback): structural edit, NOT type-checked. Be honest so the
            // agent knows to verify — and that rename was best-effort within the edited file only.
            ci_core::CommitResult::Ok { applied_ops, changed_files, .. } => Ok(format!(
                "✓ Applied {applied_ops} structural edit(s){}; {} file(s) changed. gated: false — this language has no \
                 type-checker wired up, so the edit was NOT verified to compile, and `rename` rewrote matching identifiers \
                 within the edited file only (not cross-file references). Review or run the project's own checks to confirm.\nFiles changed:\n{}",
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
        save_index(&root, &config, &updated).map_err(|e| e.to_string())
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

/// Ask the agent to re-issue with one of the candidate node ids (the disambiguation reply shared
/// by name-collision and query resolution).
fn candidate_msg(reference: &str, ids: &[String]) -> String {
    format!(
        "{reference:?} is ambiguous ({} matches) — re-issue with `name` set to one of these ids:\n{}",
        ids.len(),
        ids.join("\n")
    )
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
    } else if file.ends_with(".py") || file.ends_with(".pyi") {
        lang_fallback::outline(FbLang::Python, content)
    } else {
        content.to_string()
    }
}

// ── tool schemas ───────────────────────────────────────────────────────────
fn tools_list() -> Value {
    json!([
        {
            "name": "retrieve_context",
            "description": "Find the files and line-ranges relevant to a task. Hybrid index (BM25 + Model2Vec + symbol match) fused with RRF, expanded along the import graph. No API calls. `detailLevel` controls how much code is inlined so you may not need a separate read: `pointers` (default — just file + line-range pointers), `outline` (inline the relevant files with function/method BODIES elided — you get exact signatures, arguments, and return types but not the bodies; a 200-line file becomes ~15 lines), or `full` (inline whole files). DEFAULT to `pointers` when you just need to LOCATE code you'll then edit (apply_edits / replace_text) or expand with read_node — it's by far the cheapest. Use `outline`/`full` only when you genuinely need to read several files' code at once, not merely find them. Under `full`, files pulled in via the import graph (not direct matches) are still returned as outline — you get their signatures to call them; use read_node if you need one of their bodies.",
            "inputSchema": {"type":"object","properties":{"task":{"type":"string"},"topN":{"type":"integer"},"hops":{"type":"integer"},"detailLevel":{"type":"string","enum":["pointers","outline","full"]}},"required":["task"]}
        },
        {
            "name": "describe_architecture",
            "description": "Folder/architecture map (zero-API): per-directory file-kind patterns and detected module templates. Optional `path` scopes to a subtree.",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string"}}}
        },
        {
            "name": "find_symbols",
            "description": "Exact/substring search over indexed symbol NAMES, returning self-locating node-id handles (with kind + line range) ranked by relevance — NOT file:line. Use it to jump straight from a name to an editable handle: every result feeds directly into read_node (id=…, incl. …:body/:doc sub-nodes) or apply_edits (name=… / the id). It's the bridge between retrieve_context (fuzzy, concept→files) and grep (literal, but gives you lines you'd have to map back to a symbol). Exhaustive by default (great for audits — \"every symbol named/containing X\"), not top-k. Pass `substring:true` to match anywhere in the name; omit it for whole-name matches. Prefer this over grep when you want to act on the symbol next.",
            "inputSchema": {"type":"object","properties":{"query":{"type":"string","description":"symbol name (or fragment with substring:true)"},"substring":{"type":"boolean","description":"match anywhere in the name (default false = whole name)"}},"required":["query"]}
        },
        {
            "name": "list_anchors",
            "description": "List AST anchors (node ids + line ranges) in a file — symbols and their sub-nodes (params/return/body) — to target with apply_edits or read_node.",
            "inputSchema": {"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}
        },
        {
            "name": "read_node",
            "description": "Get the full source + metadata of ONE anchor (a symbol, or its :body / :param.N / :return / :doc sub-node) — the precise drill-down after retrieve_context `outline` elided a body. Address by `id` (a node id from list_anchors, e.g. 'src/bm25.ts#BM25.search' or '…#search:body' — self-locating, NO `file` needed), or by `name` (its file is found in the index; pass `file` only to disambiguate). To read just a body, pass the `…:body` id.",
            "inputSchema": {"type":"object","properties":{"file":{"type":"string"},"id":{"type":"string"},"name":{"type":"string"}}}
        },
        {
            "name": "apply_edits",
            "description": "Apply structured code edits atomically, type-checked over the blast radius before they land (nothing commits if it introduces a new type error, including in files that import what changed). TS + Rust are gated; Python is structural-only (`gated:false` — verify it yourself). PREFER over grep + hand-editing: `rename` and `move_file` rewrite every reference / importer across the whole codebase in ONE call — don't edit call sites yourself or grep to verify. ADDRESSING — if the task already NAMES the symbol (e.g. \"rename X to Y\", \"change the body of Z\"), go STRAIGHT to apply_edits with name=X. Do NOT call retrieve_context or list_anchors first to find it: the index resolves a bare `name` to its defining file (omit `path`), and the type-check gate verifies the blast radius — so edit by name and trust it, don't pre-read callers. A node id from list_anchors (e.g. `src/x.ts#Foo.bar`) also works and is self-locating. If you DON'T know the name, give `query` (a free-text description) instead of `name`: the server resolves it to the symbol via the index (fusing find+edit into ONE call) and applies the edit when unambiguous, else returns candidate ids. An ambiguous name/query returns candidate ids to re-issue with. So you almost never need a separate retrieve_context before editing. Each action: {path?, action, name, value, target?, oldText?, newText?}. Pick the SMALLEST edit: • `replace_text` (name=symbol, oldText=exact substring unique within it, newText) — cheapest; use it when you already know the text, with NO Read/list_anchors first. • `replace_node` + `target` for ONE sub-node: target = `body` | `return` (return-type text) | `param.N` (0-based) | `doc` (leading comment / docstring); value = the new code. • `set_body` (name=fn/method, value=new `{ … }` block) — only when rewriting most of a body. • `insert_in_body` (name=fn, value=statement; optional oldText=an existing body line to insert AFTER, else appended at the body end) / `delete_in_body` (name=fn, oldText=the statement line to remove) — statement-level body edits that leave the rest of the block intact. • `add_parameter` (name=fn, value=the new `x: T` param — appended to the list) / `set_return_type` (name=fn, value=the type — added where none exists; the server writes TS `: T` vs Rust/Python `-> T`; to CHANGE an existing return type use replace_node target:return). • `rename` (path=defining file, name=current, value=new); `move_file` (path=current, value=new path); also `insert_before` / `create_file` / `delete_file`. STRUCTURED non-code files (TOML/Markdown) — edit config/docs in the SAME atomic batch as the code that needs them (e.g. a Cargo.toml dep beside its new `use`), format-preserving and ungated: • `set_key` (path=the .toml/.md file, name=the structural path — a dotted TOML key like `dependencies.serde` or a `/`-separated Markdown heading path like `Usage/Install`, value=the new TOML value expr like `\"1.2\"` / `{ version = \"1\", features = [\"derive\"] }`, or the new Markdown section body) / `delete_key` (path + name). Actions: rename, replace_node, replace_text, set_body, insert_in_body, delete_in_body, add_parameter, set_return_type, insert_before, create_file, move_file, delete_file, set_key, delete_key.",
            "inputSchema": {"type":"object","properties":{"actions":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"action":{"type":"string"},"name":{"type":"string","description":"symbol name or a node id (file#Scope.name); the index resolves the file"},"query":{"type":"string","description":"free-text target when you don't know the name; resolved via the index, applied if unambiguous"},"value":{"type":"string"},"oldText":{"type":"string","description":"replace_text: exact substring to replace (unique within the symbol)"},"newText":{"type":"string","description":"replace_text: its replacement"},"target":{"type":"string","description":"sub-node selector: body | return | param.N | doc"}},"required":["path","action"]}},"dryRun":{"type":"boolean"}},"required":["actions"]}
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
             {model:?} (dim {dim}) — re-run `codeindex-rs index`"
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
    eprintln!("[codeindex-rs-mcp] ready for {}", server.root.display());
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
    use super::ensure_index_matches;

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
