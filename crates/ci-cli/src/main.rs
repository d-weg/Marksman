//! codeindex-rs CLI — `index` and `retrieve`. v1: TypeScript via SCIP
//! (scip-typescript) + native Model2Vec embeddings.
//!
//!   codeindex-rs index    <root>
//!   codeindex-rs retrieve <root> "<task>" [--top N] [--json]
//!
//! Model files resolve from $CI_MODEL_DIR (a Model2Vec dir with model.safetensors
//! + tokenizer.json), defaulting to the sibling Node repo's potion-code-16M.
use ci_build::build_index;
use ci_core::{Config, LanguageProvider, Manifest};
use ci_embed::StaticEmbedder;
use ci_index::{index_exists, load_index, save_index};
use ci_retrieve::{retrieve, RetrieveOptions};
use lang_fallback::{FallbackProvider, FbLang};
use lang_rust::RustProvider;
use lang_ts::TsProvider;
use std::path::{Path, PathBuf};
use std::process::exit;

fn model_dir() -> PathBuf {
    std::env::var("CI_MODEL_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        // Default to the path the README's download step uses, so the documented
        // `git clone … ~/.marksmanai/models/potion-code-16M` works without setting CI_MODEL_DIR.
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".marksmanai/models/potion-code-16M"))
            .unwrap_or_else(|_| PathBuf::from(".marksmanai/models/potion-code-16M"))
    })
}

/// Config tuned for the Rust tool: native potion embedder, separate index dir so we
/// never clobber the Node tool's `.codeindex/`.
fn rust_config(root: &Path) -> Config {
    let mut c = Config::load(root).unwrap_or_default();
    c.embedding_model = "minishlab/potion-code-16M".into();
    c.index_dir = ".codeindex-rs".into();
    c
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    exit(1);
}

/// Pick the language provider from the repo's manifests (and adjust the file globs for it),
/// so Node tooling is only invoked for a TypeScript repo. Override with `CI_LANG=rust|ts`.
/// v0 = dominant-language pick; full multi-provider dispatch is on the roadmap.
fn select_provider(root: &Path, config: &mut Config) -> Box<dyn LanguageProvider> {
    let forced = std::env::var("CI_LANG").ok();
    let has_cargo = root.join("Cargo.toml").exists();
    let has_pkg = root.join("package.json").exists();

    // Explicit override wins (incl. fallback languages by name, e.g. CI_LANG=python).
    match forced.as_deref() {
        Some("rust") => return rust_provider(root, config),
        Some("ts") | Some("typescript") => return ts_provider(root),
        Some(other) => {
            if let Some(lang) = FbLang::from_name(other) {
                return fallback_provider(root, config, lang);
            }
        }
        None => {}
    }

    // Auto-detect by the dominant manifest, then fall back to a tree-sitter language.
    if has_cargo && !has_pkg {
        rust_provider(root, config)
    } else if has_pkg || root.join("tsconfig.json").exists() {
        ts_provider(root)
    } else if let Some(lang) = FbLang::detect(root) {
        fallback_provider(root, config, lang)
    } else {
        ts_provider(root)
    }
}

fn rust_provider(root: &Path, config: &mut Config) -> Box<dyn LanguageProvider> {
    config.include = vec!["**/*.rs".into()];
    config.languages = vec!["rust".into()];
    config.exclude.push("**/target/**".into());
    eprintln!("[codeindex-rs] language: rust (tree-sitter, in-process — no Node)");
    Box::new(RustProvider::new(root))
}

fn ts_provider(root: &Path) -> Box<dyn LanguageProvider> {
    eprintln!("[codeindex-rs] language: typescript — running scip-typescript on {} …", root.display());
    Box::new(TsProvider::index(root).unwrap_or_else(|e| die(e)))
}

fn fallback_provider(root: &Path, config: &mut Config, lang: FbLang) -> Box<dyn LanguageProvider> {
    config.include = vec![format!("**/*.{}", lang_ext(lang))];
    config.languages = vec![lang.label().into()];
    eprintln!("[codeindex-rs] language: {} (tree-sitter fallback, in-process — ungated edits)", lang.label());
    Box::new(FallbackProvider::new(root, lang))
}

fn lang_ext(lang: FbLang) -> &'static str {
    match lang {
        FbLang::Python => "py",
    }
}

fn cmd_index(root: &Path) {
    let mut config = rust_config(root);
    let embedder = StaticEmbedder::load(&model_dir()).unwrap_or_else(|e| die(e));
    let dim = embedder.dim();

    let provider = select_provider(root, &mut config);

    eprintln!("[codeindex-rs] embedding + indexing …");
    let index = build_index(root, &config, provider.as_ref(), |t| {
        embedder.embed(t).unwrap_or_else(|_| vec![0.0; dim])
    })
    .unwrap_or_else(|e| die(e));

    save_index(root, &config, &index).unwrap_or_else(|e| die(e));
    eprintln!(
        "[codeindex-rs] done: {} symbols · {} chunks · dim {} -> {}/",
        index.symbols.len(),
        index.chunks.len(),
        index.meta.dims,
        config.index_dir
    );
}

fn cmd_retrieve(root: &Path, task: &str, top: Option<usize>, json: bool) {
    let config = rust_config(root);
    if !index_exists(root, &config) {
        die(format!("no index at {}/{} — run `index` first", root.display(), config.index_dir));
    }
    let index = load_index(root, &config).unwrap_or_else(|e| die(e));
    let embedder = StaticEmbedder::load(&model_dir()).unwrap_or_else(|e| die(e));
    // Model2Vec is symmetric: embed the query the same way as chunks (no bge prefix).
    let qvec = embedder.embed(task).unwrap_or_else(|e| die(e));

    let manifest = retrieve(
        root,
        task,
        &index,
        &qvec,
        &config,
        &RetrieveOptions { top_n: top, ..Default::default() },
    );

    if json {
        println!("{}", serde_json::to_string_pretty(&manifest).unwrap());
    } else {
        print!("{}", render_summary(&manifest));
    }
}

fn render_summary(m: &Manifest) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Context for: \"{}\"\n", m.task));
    out.push_str(&format!("# {} files · {}\n\n", m.entries.len(), m.root));
    for e in &m.entries {
        out.push_str(&format!(
            "{:<16} {:.3}  {}{}\n",
            e.reason,
            e.score,
            e.file,
            if e.whole_file == Some(true) { "  (whole file)" } else { "" }
        ));
        for s in &e.matched_symbols {
            out.push_str(&format!(
                "                 ↳ {} {}  L{}-{}\n",
                kind_str(s.kind),
                s.name,
                s.line_range[0],
                s.line_range[1]
            ));
        }
    }
    out
}

fn kind_str(k: ci_core::SymbolKind) -> &'static str {
    use ci_core::SymbolKind::*;
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("index") => {
            let root = args.get(1).unwrap_or_else(|| die("usage: index <root>"));
            cmd_index(Path::new(root));
        }
        Some("retrieve") => {
            let root = args.get(1).cloned().unwrap_or_else(|| die("usage: retrieve <root> <task>"));
            let mut top = None;
            let mut json = false;
            let mut parts: Vec<String> = Vec::new();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--top" => {
                        i += 1;
                        top = args.get(i).and_then(|v| v.parse().ok());
                    }
                    "--json" => json = true,
                    other => parts.push(other.to_string()),
                }
                i += 1;
            }
            let task = parts.join(" ");
            if task.is_empty() {
                die("usage: retrieve <root> <task>");
            }
            cmd_retrieve(Path::new(&root), &task, top, json);
        }
        _ => {
            eprintln!("usage:\n  codeindex-rs index <root>\n  codeindex-rs retrieve <root> \"<task>\" [--top N] [--json]");
            exit(2);
        }
    }
}
