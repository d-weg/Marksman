//! Marksman CLI — `index` and `retrieve`. v1: TypeScript via SCIP
//! (scip-typescript) + native Model2Vec embeddings.
//!
//!   marksman index    <root>
//!   marksman retrieve <root> "<task>" [--top N] [--json]
//!
//! Model files resolve from $CI_MODEL_DIR (a Model2Vec dir with model.safetensors
//! + tokenizer.json), defaulting to the sibling Node repo's potion-code-16M.
use ci_build::{build_index, build_registry, ProviderBuild};
use ci_core::{Config, Manifest};
use ci_embed::StaticEmbedder;
use ci_index::{index_exists, load_index, save_index};
use ci_retrieve::{retrieve, RetrieveOptions};
use ci_proto::ProcessProvider;
use lang_fallback::{FallbackProvider, FbLang};
use lang_rust::RustProvider;
use lang_ts::TsProvider;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;

fn model_dir() -> PathBuf {
    std::env::var("CI_MODEL_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        // Default to the path the README's download step uses, so the documented
        // `git clone … ~/.marksman/models/potion-code-16M` works without setting CI_MODEL_DIR.
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".marksman/models/potion-code-16M"))
            .unwrap_or_else(|_| PathBuf::from(".marksman/models/potion-code-16M"))
    })
}

/// Config tuned for the Rust tool: native potion embedder, separate index dir so we
/// never clobber the Node tool's `.codeindex/`.
fn rust_config(root: &Path) -> Config {
    let mut c = Config::load(root).unwrap_or_default();
    c.embedding_model = "minishlab/potion-code-16M".into();
    c.index_dir = ".marksman".into();
    c
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    exit(1);
}

/// Construct the provider for one language, honoring the manifest's vendored binary and
/// `CI_PROVIDER=sidecar`. Returns `None` (and warns) when a language's tooling can't start, so a
/// mixed-language index isn't sunk by one language failing. Called by [`build_registry`] once per
/// active language — so Node's `scip-typescript` only runs when the repo actually has `.ts*`.
fn make_provider(lang: &str, root: &Path, config: &Config) -> ProviderBuild {
    // `CI_PROVIDER=sidecar`: index over the protobuf wire via a `marksman-provider-<lang>` process.
    if std::env::var("CI_PROVIDER").as_deref() == Ok("sidecar") {
        if let Some(cmd) = ci_proto::sidecar_command_with(lang, root, false, config.provider_bin(lang)) {
            eprintln!("[marksman] language: {lang} (sidecar process — protobuf wire)");
            match ProcessProvider::spawn(cmd) {
                Ok(p) => return ProviderBuild::Ready(Arc::new(p)),
                Err(e) => {
                    eprintln!("[marksman] sidecar {lang} failed to start ({e}); skipping");
                    return ProviderBuild::Failed(e.to_string());
                }
            }
        }
        eprintln!("[marksman] CI_PROVIDER=sidecar but no marksman-provider-{lang} found — using in-process");
    }
    match lang {
        "rust" => {
            // Reads are in-process (no external deps); rust-analyzer gates only writes.
            if let Some(missing) = lang_rust::toolchain().describe_missing() {
                eprintln!("[marksman] warning: {missing}\n  (rust indexing/reads work; type-checked edits will fail until installed)");
            }
            eprintln!("[marksman] language: rust (tree-sitter, in-process — no Node)");
            ProviderBuild::Ready(Arc::new(RustProvider::new(root).with_scip(config.scip_enabled("rust"))))
        }
        "ts" => {
            // CI_TS_MODE ablation arms (docs/benchmarks.md): serve TS from tree-sitter instead
            // of SCIP — "treesitter" is the generic UNGATED provider (needs nothing external),
            // "treesitter-gated" keeps the warm ts-morph gate on a tree-sitter read path.
            match std::env::var("CI_TS_MODE").as_deref() {
                Ok("treesitter") => {
                    eprintln!("[marksman] language: typescript (ABLATION: generic tree-sitter, UNGATED — CI_TS_MODE=treesitter)");
                    return ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, FbLang::Ts)));
                }
                Ok("treesitter-gated") => {
                    if let Some(missing) = lang_ts::toolchain().describe_missing() {
                        eprintln!("[marksman] typescript DISABLED (gated ablation still needs the gate's toolchain):\n{missing}");
                        return ProviderBuild::Unavailable(missing);
                    }
                    eprintln!("[marksman] language: typescript (ABLATION: tree-sitter read + ts-morph gate — CI_TS_MODE=treesitter-gated)");
                    return ProviderBuild::Ready(Arc::new(lang_ts::TsTreeGated::new(root)));
                }
                _ => {}
            }
            // Check the toolchain BEFORE running any of it: a missing Node is one actionable
            // message (what + why + install), not a cryptic npx spawn error mid-index.
            if let Some(missing) = lang_ts::toolchain().describe_missing() {
                eprintln!("[marksman] typescript DISABLED:\n{missing}");
                return ProviderBuild::Unavailable(missing);
            }
            // `open` reuses the cached .codeindex/index.scip when the source fingerprint still
            // matches; scip-typescript re-runs only when the source actually changed.
            eprintln!("[marksman] language: typescript — opening scip index for {} …", root.display());
            match TsProvider::open(root) {
                Ok(p) => ProviderBuild::Ready(Arc::new(p)),
                Err(e) => {
                    eprintln!("[marksman] typescript indexing failed ({e}); skipping TS files");
                    ProviderBuild::Failed(e.to_string())
                }
            }
        }
        // Every other supported language rides the generic tree-sitter fallback: full read
        // path, ungated edits, zero external dependencies.
        other => match FbLang::from_name(other) {
            Some(fb) => {
                eprintln!("[marksman] language: {} (generic tree-sitter fallback, in-process — ungated edits)", fb.label());
                ProviderBuild::Ready(Arc::new(FallbackProvider::new(root, fb)))
            }
            None => ProviderBuild::Failed(format!("unknown language '{other}'")),
        },
    }
}

fn cmd_index(root: &Path) {
    let mut config = rust_config(root);
    ci_embed::ensure_model(&model_dir(), &config.embedding_model).unwrap_or_else(|e| die(e));
    let embedder = StaticEmbedder::load(&model_dir()).unwrap_or_else(|e| die(e));
    let dim = embedder.dim();

    // Extension → provider registry: dispatch each file to its language's provider (a mixed
    // Rust+TS+Python repo indexes fully). `cfg` is a snapshot for the constructors — build_registry
    // only rewrites include/exclude, which they don't read.
    let cfg = config.clone();
    let built = build_registry(root, &mut config, |lang| make_provider(lang, root, &cfg))
        .unwrap_or_else(|e| die(e));
    // A partial index (one language's toolchain down) still beats none for the CLI indexer, so we
    // proceed — but warn, since those files won't be indexed until the toolchain is fixed.
    if !built.failed.is_empty() {
        eprintln!(
            "[marksman] warning: skipping language(s) whose provider failed to start: {} — their files are NOT indexed",
            built.failed.join(", ")
        );
    }
    let registry = built.registry;

    // Opt-in (`rustScip` config / `CI_RUST_SCIP` env): generate the compiler-accurate Rust `use`
    // graph BEFORE indexing so it's the one persisted (import_graph reads the cache). Slow
    // (≈ cargo check); off by default. Only when a Rust provider is actually active.
    let rust_active = registry.provider_for(Path::new("_.rs")).is_some();
    if rust_active && config.scip_enabled("rust") {
        eprintln!("[marksman] scip[rust] enabled: generating rust-analyzer scip graph (one-time, ~cargo check) …");
        if let Err(e) = lang_rust::refresh_scip(root) {
            eprintln!("[marksman] scip graph unavailable ({e}); using the tree-sitter mod graph");
        }
    }

    eprintln!("[marksman] embedding + indexing …");
    let index = build_index(root, &config, &registry, |t| {
        embedder.embed(t).unwrap_or_else(|_| vec![0.0; dim])
    })
    .unwrap_or_else(|e| die(e));

    let t = std::time::Instant::now();
    save_index(root, &config, &index).unwrap_or_else(|e| die(e));
    if std::env::var("CI_TIMING").is_ok() {
        eprintln!("[timing] save_index {:.3}s", t.elapsed().as_secs_f64());
    }
    eprintln!(
        "[marksman] done: {} symbols · {} chunks · dim {} -> {}/",
        index.symbols.len(),
        index.chunks.len(),
        index.meta.dims,
        config.index_dir
    );
}

/// `doctor` — the human entry to the dependency layer: which languages this repo actually
/// contains, what each one needs from the machine, what's installed (with versions), what's
/// missing (with install instructions), plus embedding-model and index status. Read-only —
/// probes `--version`s, runs nothing heavy, fetches nothing. Exits non-zero when a PRESENT
/// language is missing a required tool, so scripts can gate on it.
fn cmd_doctor(root: &Path) {
    let config = rust_config(root);
    let present = ci_walk::present_langs(root, &config).unwrap_or_else(|e| die(e));
    let has = |l: ci_walk::Lang| present.contains(&l);
    println!("marksman doctor — {}\n", root.display());

    let mut unhealthy = false;
    let mut section = |report: ci_core::ToolchainReport, note: Option<&str>| {
        println!("[{}]", report.lang);
        for t in &report.tools {
            match &t.found {
                Some(v) => println!("  ok       {} ({v})", t.tool),
                None => {
                    unhealthy = true;
                    println!("  MISSING  {} — needed for {}\n           install: {}", t.tool, t.needed_for, t.install);
                }
            }
        }
        if let Some(n) = note {
            println!("  note     {n}");
        }
        println!();
    };

    if has(ci_walk::Lang::Ts) || has(ci_walk::Lang::Tsx) {
        section(lang_ts::toolchain(), Some("scip-typescript / ts-morph are fetched automatically once node+npx exist"));
    }
    if has(ci_walk::Lang::Rust) {
        section(lang_rust::toolchain(), Some("reads (structure/import graph) are in-process and need nothing external"));
    }
    let fallback_langs: Vec<&str> = [
        (ci_walk::Lang::Python, "python"),
        (ci_walk::Lang::Js, "javascript"),
        (ci_walk::Lang::Go, "go"),
        (ci_walk::Lang::Java, "java"),
        (ci_walk::Lang::Ruby, "ruby"),
        (ci_walk::Lang::C, "c"),
        (ci_walk::Lang::Cpp, "cpp"),
    ]
    .iter()
    .filter(|(l, _)| has(*l))
    .map(|(_, n)| *n)
    .collect();
    if !fallback_langs.is_empty() {
        println!(
            "[{}]\n  ok       no external tooling (generic in-process tree-sitter; edits are ungated)\n",
            fallback_langs.join(", ")
        );
    }
    if present.iter().all(|l| !l.is_code()) {
        println!("no supported source languages detected under {}\n", root.display());
    }

    println!("[embedding model]");
    let md = model_dir();
    if md.join("model.safetensors").is_file() {
        println!("  ok       {}", md.display());
    } else {
        unhealthy = true;
        println!("  MISSING  {} — needed for retrieval (BM25+vector index)\n           install: see README \"Get the embedding model\" (or set CI_MODEL_DIR)", md.display());
    }

    println!("\n[index]");
    if index_exists(root, &config) {
        println!("  ok       {}/{}", root.display(), config.index_dir);
    } else {
        println!("  none     run `marksman index {}`", root.display());
    }

    if unhealthy {
        println!("\nstatus: MISSING DEPENDENCIES (see install lines above)");
        exit(1);
    }
    println!("\nstatus: healthy");
}

fn cmd_retrieve(root: &Path, task: &str, top: Option<usize>, json: bool) {
    let config = rust_config(root);
    if !index_exists(root, &config) {
        die(format!("no index at {}/{} — run `index` first", root.display(), config.index_dir));
    }
    let index = load_index(root, &config).unwrap_or_else(|e| die(e));
    ci_embed::ensure_model(&model_dir(), &config.embedding_model).unwrap_or_else(|e| die(e));
    let embedder = StaticEmbedder::load(&model_dir()).unwrap_or_else(|e| die(e));
    if embedder.dim() != index.meta.dims || index.meta.model != config.embedding_model {
        die(format!(
            "index was built with model {:?} (dim {}) but this run uses {:?} (dim {}) — re-run `index`",
            index.meta.model, index.meta.dims, config.embedding_model, embedder.dim()
        ));
    }
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
                "                 ↳ {} {}  L{}-{}  [{}]\n",
                s.kind.as_str(),
                s.name,
                s.line_range[0],
                s.line_range[1],
                s.node_id
            ));
        }
    }
    out
}

/// One labeled retrieval case: a task and the repo-relative files that SHOULD surface for it.
struct EvalCase {
    task: String,
    expect_files: Vec<String>,
}

/// Reciprocal rank of the first expected file in `ranked` (0 if none), and whether that rank is
/// within `k` (hit@k). Pure — the scoreable heart of the eval, independent of embedding/retrieval.
fn score_case(ranked: &[String], expect: &[String], k: usize) -> (bool, f64) {
    for (i, f) in ranked.iter().enumerate() {
        if expect.iter().any(|e| e == f) {
            let rank = i + 1;
            return (rank <= k, 1.0 / rank as f64);
        }
    }
    (false, 0.0)
}

/// Run a labeled eval set against the index and report overlap@k + MRR — the gate for any future
/// ranking-weight change (roadmap Invariants). The eval file is a JSON array of
/// `{ "task": "...", "expectFiles": ["path", ...] }`.
fn cmd_eval(root: &Path, eval_path: &Path, k: usize) {
    let config = rust_config(root);
    if !index_exists(root, &config) {
        die(format!("no index at {}/{} — run `index` first", root.display(), config.index_dir));
    }
    let index = load_index(root, &config).unwrap_or_else(|e| die(e));
    ci_embed::ensure_model(&model_dir(), &config.embedding_model).unwrap_or_else(|e| die(e));
    let embedder = StaticEmbedder::load(&model_dir()).unwrap_or_else(|e| die(e));
    if embedder.dim() != index.meta.dims || index.meta.model != config.embedding_model {
        die("index model/dim differs from the embedder — re-run `index`");
    }

    let raw = std::fs::read_to_string(eval_path).unwrap_or_else(|e| die(format!("reading {}: {e}", eval_path.display())));
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| die(format!("parsing eval json: {e}")));
    let cases: Vec<EvalCase> = json
        .as_array()
        .unwrap_or_else(|| die("eval json must be an array"))
        .iter()
        .map(|c| EvalCase {
            task: c["task"].as_str().unwrap_or_default().to_string(),
            expect_files: c["expectFiles"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        })
        .collect();

    let (mut hits, mut rr_sum) = (0usize, 0.0f64);
    for case in &cases {
        let qvec = embedder.embed(&case.task).unwrap_or_else(|e| die(e));
        let manifest = retrieve(root, &case.task, &index, &qvec, &config, &RetrieveOptions { top_n: Some(k), ..Default::default() });
        let ranked: Vec<String> = manifest.entries.iter().map(|e| e.file.clone()).collect();
        let (hit, rr) = score_case(&ranked, &case.expect_files, k);
        if hit {
            hits += 1;
        }
        rr_sum += rr;
        println!("{} rr={rr:.2}  {}", if hit { "✓" } else { "✗" }, case.task);
    }
    let n = cases.len().max(1) as f64;
    println!(
        "\n{} cases · overlap@{k}: {:.1}% ({}/{}) · MRR: {:.3}",
        cases.len(),
        100.0 * hits as f64 / n,
        hits,
        cases.len(),
        rr_sum / n
    );
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
        Some("doctor") => {
            let root = args.get(1).map(String::as_str).unwrap_or(".");
            cmd_doctor(Path::new(root));
        }
        Some("eval") => {
            let root = args.get(1).cloned().unwrap_or_else(|| die("usage: eval <root> <eval.json> [--top N]"));
            let eval = args.get(2).cloned().unwrap_or_else(|| die("usage: eval <root> <eval.json> [--top N]"));
            let mut k = 8usize;
            let mut i = 3;
            while i < args.len() {
                if args[i] == "--top" {
                    i += 1;
                    k = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(k);
                }
                i += 1;
            }
            cmd_eval(Path::new(&root), Path::new(&eval), k);
        }
        _ => {
            eprintln!("usage:\n  marksman index <root>\n  marksman retrieve <root> \"<task>\" [--top N] [--json]\n  marksman doctor [<root>]\n  marksman eval <root> <eval.json> [--top N]");
            exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::score_case;

    #[test]
    fn score_case_rank_and_hit() {
        let ranked = vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()];
        // first expected file is at rank 2 → rr = 0.5, hit@k for k>=2.
        assert_eq!(score_case(&ranked, &["b.rs".into()], 8), (true, 0.5));
        // hit@k is false when the match falls outside k, but the reciprocal rank still reflects it.
        assert_eq!(score_case(&ranked, &["c.rs".into()], 2), (false, 1.0 / 3.0));
        // no expected file present → miss.
        assert_eq!(score_case(&ranked, &["z.rs".into()], 8), (false, 0.0));
    }
}
