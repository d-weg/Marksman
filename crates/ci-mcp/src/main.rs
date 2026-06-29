//! codeindex-rs MCP server (stdio, JSON-RPC 2.0, newline-delimited). Exposes the
//! input tool (retrieve_context, describe_architecture) and the output tools
//! (list_anchors, apply_edits). Launch per repo:
//!   codeindex-rs-mcp --root /path/to/repo   (or $CODEINDEX_ROOT, or cwd)
//!
//! The server is pure-Rust orchestration; all language/external tooling is behind
//! the `lang-ts` provider.
use ci_arch::{build_architecture, format_architecture};
use ci_core::{Config, EditOpts, LanguageProvider, Manifest, Node, SymbolKind};
use ci_edit::{action_to_op, resolve_in, Action};
use ci_embed::StaticEmbedder;
use ci_index::{index_exists, load_index};
use ci_retrieve::{retrieve, RetrieveOptions};
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
}

impl AnyProvider {
    /// Warm the write engine — TS only (Rust has no edit gate yet).
    fn prewarm(&self) {
        if let AnyProvider::Ts(t) = self {
            t.prewarm();
        }
    }
}

impl LanguageProvider for AnyProvider {
    fn granularity(&self) -> ci_core::Granularity {
        match self {
            AnyProvider::Ts(t) => t.granularity(),
            AnyProvider::Rust(r) => r.granularity(),
        }
    }
    fn structure(&self, file: &Path) -> ci_core::Result<Vec<Node>> {
        match self {
            AnyProvider::Ts(t) => t.structure(file),
            AnyProvider::Rust(r) => r.structure(file),
        }
    }
    fn import_graph(&self) -> ci_core::Result<ci_core::ImportGraph> {
        match self {
            AnyProvider::Ts(t) => t.import_graph(),
            AnyProvider::Rust(r) => r.import_graph(),
        }
    }
    fn apply_edits(&self, ops: &[ci_core::EditOp], opts: &EditOpts) -> ci_core::Result<ci_core::CommitResult> {
        match self {
            AnyProvider::Ts(t) => t.apply_edits(ops, opts),
            AnyProvider::Rust(r) => r.apply_edits(ops, opts),
        }
    }
}

/// Build the provider for `root`: Rust (in-process tree-sitter, no Node) when it looks like a
/// Rust repo, else TypeScript (scip-typescript). `CI_LANG=rust|ts` overrides.
fn build_provider(root: &Path) -> Result<AnyProvider, String> {
    let forced = std::env::var("CI_LANG").ok();
    let rust = match forced.as_deref() {
        Some("rust") => true,
        Some("ts") | Some("typescript") => false,
        _ => root.join("Cargo.toml").exists() && !root.join("package.json").exists(),
    };
    if rust {
        eprintln!("[codeindex-rs-mcp] language: rust (tree-sitter, in-process — no Node)");
        Ok(AnyProvider::Rust(RustProvider::new(root)))
    } else {
        Ok(AnyProvider::Ts(TsProvider::index(root).map_err(|e| e.to_string())?))
    }
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
        // `git clone … ~/.codegraph/models/potion-code-16M` works without setting CI_MODEL_DIR.
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".codegraph/models/potion-code-16M"))
            .unwrap_or_else(|_| PathBuf::from(".codegraph/models/potion-code-16M"))
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
        Ok(render_summary(&manifest))
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
            ci_core::CommitResult::Ok { applied_ops, changed_files, .. } => Ok(format!(
                "✓ Applied {applied_ops} edit(s){}; {} file(s) changed; type-checked clean — no new type errors anywhere, \
                 including files that import what changed. rename/move already updated every reference/import across the \
                 whole codebase, so this change is COMPLETE — do not grep, re-read, or hand-edit call sites to verify.\nFiles changed:\n{}",
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
            "description": "Find the files and line-ranges relevant to a task. Hybrid index (BM25 + Model2Vec + symbol match) fused with RRF, expanded along the import graph. No API calls.",
            "inputSchema": {"type":"object","properties":{"task":{"type":"string"},"topN":{"type":"integer"},"hops":{"type":"integer"}},"required":["task"]}
        },
        {
            "name": "describe_architecture",
            "description": "Folder/architecture map (zero-API): per-directory file-kind patterns and detected module templates. Optional `path` scopes to a subtree.",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string"}}}
        },
        {
            "name": "list_anchors",
            "description": "List AST anchors (node ids + line ranges) in a TS file — symbols and their sub-nodes (params/return/body) — to target with apply_edits.",
            "inputSchema": {"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}
        },
        {
            "name": "apply_edits",
            "description": "Apply structured code edits atomically, gated by the TS type-checker (nothing lands if it would introduce a new type error — including in OTHER files that import what you changed). PREFER THIS over grepping + hand-editing: `rename` rewrites the definition AND every reference across the WHOLE codebase in this one call; `move_file` updates every importer. So to rename a symbol everywhere you make ONE call — do not search for or edit call sites yourself, and no verification grep is needed afterward. `actions`: [{path, action, name, value}]. For `rename`: path = the file that DEFINES the symbol (use retrieve_context to find it), name = its current name, value = the new name. For `replace_node`/`insert_before`: name = the target symbol, value = the new code. For `move_file`: path = current file, value = new path. Actions: rename, replace_node, insert_before, create_file, move_file, delete_file.",
            "inputSchema": {"type":"object","properties":{"actions":{"type":"array"},"dryRun":{"type":"boolean"}},"required":["actions"]}
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
