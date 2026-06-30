//! codeindex-rs MCP server (stdio, JSON-RPC 2.0, newline-delimited). Exposes the
//! input tool (retrieve_context, describe_architecture) and the output tools
//! (list_anchors, apply_edits). Launch per repo:
//!   codeindex-rs-mcp --root /path/to/repo   (or $CODEINDEX_ROOT, or cwd)
//!
//! The server is pure-Rust orchestration; all language/external tooling is behind
//! the `lang-ts` provider.
use ci_arch::{build_architecture, format_architecture};
use ci_core::{Config, EditOpts, LanguageProvider, Manifest, Node, NodeKind, SymbolKind};
use ci_edit::{action_to_op, resolve_in, Action};
use ci_embed::StaticEmbedder;
use ci_index::{index_exists, load_index};
use ci_retrieve::{retrieve, RetrieveOptions};
use lang_fallback::{FallbackProvider, FbLang};
use lang_rust::RustProvider;
use lang_ts::TsProvider;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The active provider for this repo. Both variants are cheap to clone (Arc-shared / a
/// PathBuf), so the server hands one out of its lock per call.
#[derive(Clone)]
enum AnyProvider {
    Ts(TsProvider),
    Rust(RustProvider),
    Fallback(FallbackProvider),
}

impl AnyProvider {
    /// Warm the write engine on a background thread (tsserver/ts-morph for TS, rust-analyzer
    /// for Rust), so the first `apply_edits` is fast. The tree-sitter fallback has no engine
    /// to warm.
    fn prewarm(&self) {
        match self {
            AnyProvider::Ts(t) => t.prewarm(),
            AnyProvider::Rust(r) => r.prewarm(),
            AnyProvider::Fallback(_) => {}
        }
    }

    /// Whether this provider type-checks its edits over the blast radius. The tree-sitter
    /// fallback does not (no compiler/LSP) — its edits are structural-only.
    fn gated(&self) -> bool {
        !matches!(self, AnyProvider::Fallback(_))
    }
}

impl LanguageProvider for AnyProvider {
    fn granularity(&self) -> ci_core::Granularity {
        match self {
            AnyProvider::Ts(t) => t.granularity(),
            AnyProvider::Rust(r) => r.granularity(),
            AnyProvider::Fallback(f) => f.granularity(),
        }
    }
    fn structure(&self, file: &Path) -> ci_core::Result<Vec<Node>> {
        match self {
            AnyProvider::Ts(t) => t.structure(file),
            AnyProvider::Rust(r) => r.structure(file),
            AnyProvider::Fallback(f) => f.structure(file),
        }
    }
    fn import_graph(&self) -> ci_core::Result<ci_core::ImportGraph> {
        match self {
            AnyProvider::Ts(t) => t.import_graph(),
            AnyProvider::Rust(r) => r.import_graph(),
            AnyProvider::Fallback(f) => f.import_graph(),
        }
    }
    fn apply_edits(&self, ops: &[ci_core::EditOp], opts: &EditOpts) -> ci_core::Result<ci_core::CommitResult> {
        match self {
            AnyProvider::Ts(t) => t.apply_edits(ops, opts),
            AnyProvider::Rust(r) => r.apply_edits(ops, opts),
            AnyProvider::Fallback(f) => f.apply_edits(ops, opts),
        }
    }
}

/// Build the provider for `root`: Rust (in-process tree-sitter, no Node) when it looks like a
/// Rust repo, else TypeScript (scip-typescript). `CI_LANG=rust|ts` overrides.
fn build_provider(root: &Path) -> Result<AnyProvider, String> {
    let forced = std::env::var("CI_LANG").ok();
    let has_cargo = root.join("Cargo.toml").exists();
    let has_pkg = root.join("package.json").exists();

    // Explicit override (incl. fallback languages by name, e.g. CI_LANG=python).
    match forced.as_deref() {
        Some("rust") => return Ok(rust(root)),
        Some("ts") | Some("typescript") => return ts(root),
        Some(other) => {
            if let Some(lang) = FbLang::from_name(other) {
                return Ok(fallback(root, lang));
            }
        }
        None => {}
    }

    if has_cargo && !has_pkg {
        Ok(rust(root))
    } else if has_pkg || root.join("tsconfig.json").exists() {
        ts(root)
    } else if let Some(lang) = FbLang::detect(root) {
        Ok(fallback(root, lang))
    } else {
        ts(root)
    }
}

fn rust(root: &Path) -> AnyProvider {
    eprintln!("[codeindex-rs-mcp] language: rust (tree-sitter, in-process — no Node)");
    AnyProvider::Rust(RustProvider::new(root))
}

fn ts(root: &Path) -> Result<AnyProvider, String> {
    Ok(AnyProvider::Ts(TsProvider::index(root).map_err(|e| e.to_string())?))
}

fn fallback(root: &Path, lang: FbLang) -> AnyProvider {
    eprintln!(
        "[codeindex-rs-mcp] language: {} (tree-sitter fallback, in-process — edits are ungated)",
        lang.label()
    );
    AnyProvider::Fallback(FallbackProvider::new(root, lang))
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
    provider: Arc<Mutex<Option<AnyProvider>>>,
    embedder: Option<StaticEmbedder>,
}

impl Server {
    fn new(root: PathBuf) -> Self {
        let mut config = Config::load(&root).unwrap_or_default();
        config.embedding_model = "minishlab/potion-code-16M".into();
        config.index_dir = ".codeindex-rs".into();
        Server { root, config, provider: Arc::new(Mutex::new(None)), embedder: None }
    }

    /// Build the provider for the repo AND warm the write engine on a background thread at
    /// startup — so the first output-tool call finds it ready. For a TS repo this runs
    /// scip-typescript + warms the language server; for a Rust repo it's instant (in-process
    /// tree-sitter, no Node). Holding the provider lock across the build means a tool that
    /// needs it mid-build waits, not races.
    fn start_prewarm(&self) {
        let slot = self.provider.clone();
        let root = self.root.clone();
        std::thread::spawn(move || {
            let Ok(mut g) = slot.lock() else { return };
            if g.is_some() {
                return;
            }
            if let Ok(p) = build_provider(&root) {
                p.prewarm();
                *g = Some(p);
            }
        });
    }

    /// Get the provider, building it if `start_prewarm` hasn't finished. Returns a cheap
    /// clone so the caller doesn't hold the lock. Needed by the output tools only.
    fn provider(&self) -> Result<AnyProvider, String> {
        let mut g = self.provider.lock().map_err(|_| "provider lock poisoned".to_string())?;
        if g.is_none() {
            let p = build_provider(&self.root)?;
            p.prewarm();
            *g = Some(p);
        }
        Ok(g.as_ref().unwrap().clone())
    }

    fn embedder(&mut self) -> Result<&StaticEmbedder, String> {
        if self.embedder.is_none() {
            self.embedder = Some(StaticEmbedder::load(&model_dir()).map_err(|e| e.to_string())?);
        }
        Ok(self.embedder.as_ref().unwrap())
    }

    fn retrieve_context(&mut self, args: &Value) -> Result<String, String> {
        let task = args["task"].as_str().ok_or("`task` is required")?.to_string();
        if !index_exists(&self.root, &self.config) {
            return Err("no index — run `codeindex-rs index <root>` first".into());
        }
        let index = load_index(&self.root, &self.config).map_err(|e| e.to_string())?;
        let qvec = self.embedder()?.embed(&task).map_err(|e| e.to_string())?;
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

    fn list_anchors(&mut self, args: &Value) -> Result<String, String> {
        let file = args["file"].as_str().ok_or("`file` is required")?.to_string();
        let nodes = self.provider()?.structure(Path::new(&file)).map_err(|e| e.to_string())?;
        let mut out = String::new();
        for n in &nodes {
            write_anchors(n, &mut out, 0);
        }
        Ok(if out.is_empty() { "(no symbols)".into() } else { out })
    }

    /// Drill-down: the full source + metadata of ONE anchor (a symbol or its `:body`/`:param`/
    /// `:return` sub-node) — what you call after `retrieve_context` outline elided a body.
    fn read_node(&mut self, args: &Value) -> Result<String, String> {
        let file = args["file"].as_str().ok_or("`file` is required")?.to_string();
        let nodes = self.provider()?.structure(Path::new(&file)).map_err(|e| e.to_string())?;
        let id = if let Some(id) = args["id"].as_str() {
            id.to_string()
        } else if let Some(name) = args["name"].as_str() {
            resolve_in(&nodes, name).ok_or_else(|| format!("symbol '{name}' not found in {file}"))?
        } else {
            return Err("provide `id` (an anchor id from list_anchors) or `name`".into());
        };
        let node = find_node(&nodes, &id).ok_or_else(|| format!("anchor '{id}' not found in {file}"))?;
        let content = std::fs::read_to_string(self.root.join(&file)).map_err(|e| e.to_string())?;
        let text = slice_lines(&content, node.range.start_line, node.range.end_line);
        let kind = match &node.kind {
            NodeKind::Symbol(k) => kind_str(*k).to_string(),
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
        let provider = self.provider()?;

        let mut ops = Vec::new();
        for a in &actions {
            let action = Action {
                path: a["path"].as_str().unwrap_or("").to_string(),
                action: a["action"].as_str().unwrap_or("").to_string(),
                target: a["target"].as_str().map(str::to_string),
                name: a["name"].as_str().map(str::to_string),
                value: a["value"].as_str().map(str::to_string),
                old_text: a["oldText"].as_str().map(str::to_string),
                new_text: a["newText"].as_str().map(str::to_string),
            };
            let resolve = |p: &str, _t: Option<&str>, n: Option<&str>| {
                let nodes = provider.structure(Path::new(p)).unwrap_or_default();
                n.and_then(|name| resolve_in(&nodes, name))
            };
            ops.push(action_to_op(&action, resolve).map_err(|e| e.to_string())?);
        }

        let opts = EditOpts { write: !dry_run, dry_run, tsconfig: None };
        let res = provider.apply_edits(&ops, &opts).map_err(|e| e.to_string())?;
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
            out.push_str(&format!("                 ↳ {} {}  L{}-{}\n", kind_str(s.kind), s.name, s.line_range[0], s.line_range[1]));
        }
    }
    out
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

fn kind_str(k: SymbolKind) -> &'static str {
    use SymbolKind::*;
    match k {
        Function => "function",
        Class => "class",
        Interface => "interface",
        Enum => "enum",
        TypeAlias => "type",
        Variable => "var",
        Method => "method",
        Struct => "struct",
        Doc => "doc",
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
            "name": "list_anchors",
            "description": "List AST anchors (node ids + line ranges) in a file — symbols and their sub-nodes (params/return/body) — to target with apply_edits or read_node.",
            "inputSchema": {"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}
        },
        {
            "name": "read_node",
            "description": "Get the full source + metadata of ONE anchor (a symbol, or its :body / :param.N / :return sub-node) — the precise drill-down after retrieve_context `outline` elided a body. Provide `file` and either `id` (an anchor id from list_anchors, e.g. 'src/bm25.rs#Bm25.add_doc' or '…#add_doc:body') or `name` (the symbol's name).",
            "inputSchema": {"type":"object","properties":{"file":{"type":"string"},"id":{"type":"string"},"name":{"type":"string"}},"required":["file"]}
        },
        {
            "name": "apply_edits",
            "description": "Apply structured code edits atomically, gated by the language type-checker (nothing lands if it would introduce a new type error — including in OTHER files that import what you changed). NOTE: TypeScript and Rust are type-checked; some languages (e.g. Python) run a structural-only fallback whose result says `gated: false` — verify those yourself. PREFER THIS over grepping + hand-editing: `rename` rewrites the definition AND every reference across the WHOLE codebase in this one call; `move_file` updates every importer. So to rename a symbol everywhere you make ONE call — do not search for or edit call sites yourself, and no verification grep is needed afterward. `actions`: [{path, action, name, value, target?}]. For `rename`: path = the file that DEFINES the symbol (use retrieve_context to find it), name = its current name, value = the new name. For `replace_node`/`insert_before`: name = the target symbol, value = the new code. For `move_file`: path = current file, value = new path. CHOOSING THE RIGHT EDIT (smallest payload wins): if you already KNOW the exact text to change — it's in the task, or you saw it via retrieve_context/read_node — use `replace_text` (name = the symbol, oldText = the exact substring, unique within it, newText = its replacement). It needs NO separate read and re-emits NOTHING — do not Read the file or call list_anchors first just to use it. Only fall back to `set_body` (name = the function/method, value = its whole new `{ … }` block) when you're rewriting MOST of the body — it forces you to know and re-emit the entire body. For one sub-node, set `target` on `replace_node`: `target:\"body\"`, `target:\"return\"` (return-type text), or `target:\"param.N\"` (Nth param, 0-based). Actions: rename, replace_node, replace_text, set_body, insert_before, create_file, move_file, delete_file.",
            "inputSchema": {"type":"object","properties":{"actions":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"action":{"type":"string"},"name":{"type":"string"},"value":{"type":"string"},"oldText":{"type":"string","description":"replace_text: exact substring to replace (unique within the symbol)"},"newText":{"type":"string","description":"replace_text: its replacement"},"target":{"type":"string","description":"optional sub-node selector for surgical edits: body | return | param.N"}},"required":["path","action"]}},"dryRun":{"type":"boolean"}},"required":["actions"]}
        }
    ])
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
