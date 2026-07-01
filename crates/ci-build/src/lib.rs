//! ci-build — the index build pipeline. Language-blind: it walks the repo, asks a
//! [`LanguageProvider`] for each file's structure + the import graph, slices chunk
//! text by node range, embeds (via an injected closure), builds BM25, and assembles
//! an [`IndexData`]. The embedder is injected so this crate stays free of the model
//! and is unit-testable with a trivial stand-in (mirrors how ci-retrieve injects the
//! query vector).
use ci_core::{Config, Result, SymbolKind};
use ci_index::{
    build_graph, tokenize, Adjacency, Bm25, ChunkMeta, FileRecord, IndexData, IndexMeta,
    PackageMeta, SymbolEntry,
};
use ci_walk::{detect_workspace, discover, Lang};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

mod registry;
pub use registry::{build_registry, ProviderRegistry};

struct Item {
    sym: SymbolEntry,
    chunk: ChunkMeta,
    text: String,
}

fn now_millis() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0).to_string()
}

fn mtime_ms(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Inclusive 1-based line slice of `text`.
fn slice_lines(text: &str, start_line: u32, end_line: u32) -> String {
    if start_line == 0 {
        return String::new();
    }
    text.lines()
        .skip(start_line as usize - 1)
        .take((end_line.saturating_sub(start_line) + 1) as usize)
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn heading(line: &str) -> Option<String> {
    let l = line.trim_start_matches(' ');
    let hashes = l.chars().take_while(|c| *c == '#').count();
    if (1..=3).contains(&hashes) {
        let rest = &l[hashes..];
        if rest.starts_with([' ', '\t']) {
            let title = rest.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

/// Split a markdown file into heading sections -> doc chunks (port of extractDocChunks).
fn doc_items(rel: &str, content: &str) -> Vec<Item> {
    let base = Path::new(rel)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.to_string());
    struct Sec {
        title: String,
        start: usize,
        body: Vec<String>,
    }
    let mut sections: Vec<Sec> = Vec::new();
    let mut cur = Sec { title: base.clone(), start: 1, body: vec![] };
    for (i, line) in content.split('\n').enumerate() {
        if let Some(t) = heading(line) {
            if cur.body.iter().any(|l| !l.trim().is_empty()) {
                sections.push(cur);
            }
            cur = Sec { title: t, start: i + 1, body: vec![] };
        }
        cur.body.push(line.to_string());
    }
    if cur.body.iter().any(|l| !l.trim().is_empty()) {
        sections.push(cur);
    }

    sections
        .into_iter()
        .map(|s| {
            let end_line = s.start + s.body.len() - 1;
            let name = truncate_chars(&format!("{base}:{}", s.title), 80);
            let id = format!("{rel}#{}@{}", s.title, s.start);
            let sym = SymbolEntry {
                id: id.clone(),
                name: name.clone(),
                kind: SymbolKind::Doc,
                file: rel.to_string(),
                pkg: "docs".to_string(),
                start_line: s.start as u32,
                end_line: end_line as u32,
                signature: Some(truncate_chars(&s.title, 140)),
            };
            let chunk = ChunkMeta {
                id,
                symbol: name,
                kind: SymbolKind::Doc,
                file: rel.to_string(),
                pkg: "docs".to_string(),
                start_line: s.start as u32,
                end_line: end_line as u32,
            };
            let text = truncate_chars(&format!("doc {}\n{}", s.title, s.body.join("\n")), 1500);
            Item { sym, chunk, text }
        })
        .collect()
}

/// Build a fresh index. `embed` maps chunk text -> a normalized vector (all the
/// same dimension). Returns the assembled [`IndexData`]; the caller persists it.
pub fn build_index(
    root: &Path,
    config: &Config,
    registry: &ProviderRegistry,
    embed: impl Fn(&str) -> Vec<f32>,
) -> Result<IndexData> {
    let files = discover(root, config)?;
    let ws = detect_workspace(root)?;

    let mut items: Vec<Item> = Vec::new();
    let mut file_records: BTreeMap<String, FileRecord> = BTreeMap::new();

    for f in &files {
        let rel = f.rel.to_string_lossy().replace('\\', "/");
        let abs = root.join(&f.rel);

        // Per-file provider dispatch: a mixed repo indexes each file with its own language's
        // provider (skip a code file whose language has no active provider).
        if f.lang.is_code() {
            let Some(provider) = registry.provider_for(&f.rel) else { continue };
            let pkg = ws.package_for(&f.rel).map(|p| p.name.clone()).unwrap_or_else(|| "root".into());
            let content = std::fs::read_to_string(&abs).unwrap_or_default();
            for node in provider.structure(&f.rel)? {
                node_items(&node, &rel, &pkg, &content, &mut items);
            }
            file_records.insert(rel.clone(), FileRecord { mtime_ms: mtime_ms(&abs), pkg });
        } else if f.is_doc {
            let content = std::fs::read_to_string(&abs).unwrap_or_default();
            items.extend(doc_items(&rel, &content));
            file_records.insert(rel.clone(), FileRecord { mtime_ms: mtime_ms(&abs), pkg: "docs".into() });
        }
    }

    // Embed (injected) — vectors are row-aligned with `items`.
    let vecs: Vec<Vec<f32>> = items.iter().map(|i| embed(&i.text)).collect();
    let dims = vecs.first().map(|v| v.len()).unwrap_or(256);
    let mut vectors = Vec::with_capacity(vecs.len() * dims);
    for v in &vecs {
        vectors.extend_from_slice(v);
    }

    // BM25.
    let mut bm25 = Bm25::new();
    for i in &items {
        bm25.add_doc(&i.chunk.id, &i.chunk.file, &tokenize(&i.text));
    }

    // Import graph: the UNION of each active provider's graph. Import edges are within-language
    // and each provider scopes its graph to its own files, so the file keys are disjoint across
    // languages — a plain union, no cross-language merge.
    let forward = forward_adjacency(registry)?;

    let meta = IndexMeta {
        version: ci_index::INDEX_VERSION,
        created_at: now_millis(),
        updated_at: now_millis(),
        model: config.embedding_model.clone(),
        dims,
        root: root.display().to_string(),
        is_monorepo: ws.is_monorepo,
        packages: ws.packages.iter().map(package_meta).collect(),
        package_names: ws.packages.iter().map(|p| p.name.clone()).collect(),
        files: file_records,
    };

    let symbols = items.iter().map(|i| i.sym.clone()).collect();
    let chunks = items.iter().map(|i| i.chunk.clone()).collect();
    let graph = build_graph(forward.clone());

    Ok(IndexData { meta, symbols, chunks, vectors, forward, graph, bm25 })
}

/// Incrementally refresh an existing index for a set of changed files (the
/// reindex-on-commit path). Keeps every unaffected chunk/vector/symbol, re-extracts
/// and re-embeds only the changed files, and takes the fresh whole-project import
/// graph from the (already-refreshed) provider. Deleted files are dropped.
pub fn update_index(
    root: &Path,
    registry: &ProviderRegistry,
    embed: impl Fn(&str) -> Vec<f32>,
    mut data: IndexData,
    changed: &[String],
) -> Result<IndexData> {
    let changed_set: HashSet<String> = changed.iter().map(|s| s.replace('\\', "/")).collect();
    let ws = detect_workspace(root)?;
    let dims = data.meta.dims;

    // 1. Keep rows for unaffected files (vectors stay row-aligned with chunks).
    let mut chunks: Vec<ChunkMeta> = Vec::new();
    let mut vectors: Vec<f32> = Vec::new();
    for (i, c) in data.chunks.iter().enumerate() {
        if !changed_set.contains(&c.file) {
            chunks.push(c.clone());
            vectors.extend_from_slice(&data.vectors[i * dims..(i + 1) * dims]);
        }
    }
    let mut symbols: Vec<SymbolEntry> =
        data.symbols.iter().filter(|s| !changed_set.contains(&s.file)).cloned().collect();

    // 2. Re-extract + re-embed changed files that still exist.
    let mut new_items: Vec<Item> = Vec::new();
    for rel in &changed_set {
        let abs = root.join(rel);
        if !abs.exists() {
            continue; // deletion -> nothing to add back
        }
        let lang = Lang::of(Path::new(rel));
        if lang.is_code() {
            let Some(provider) = registry.provider_for(Path::new(rel)) else { continue };
            let pkg =
                ws.package_for(Path::new(rel)).map(|p| p.name.clone()).unwrap_or_else(|| "root".into());
            let content = std::fs::read_to_string(&abs).unwrap_or_default();
            for node in provider.structure(Path::new(rel))? {
                node_items(&node, rel, &pkg, &content, &mut new_items);
            }
        } else if matches!(lang, Lang::Markdown) {
            let content = std::fs::read_to_string(&abs).unwrap_or_default();
            new_items.extend(doc_items(rel, &content));
        }
    }
    for item in &new_items {
        let v = embed(&item.text);
        debug_assert_eq!(v.len(), dims, "embedder dim must match the index");
        chunks.push(item.chunk.clone());
        symbols.push(item.sym.clone());
        vectors.extend_from_slice(&v);
    }

    // 3. BM25: drop changed-file docs, add the re-extracted ones.
    data.bm25.remove_by_files(&changed_set);
    for item in &new_items {
        data.bm25.add_doc(&item.chunk.id, &item.chunk.file, &tokenize(&item.text));
    }

    // 4. Import graph: SCIP is whole-project, so take the fresh full union graph.
    let forward = forward_adjacency(registry)?;
    let graph = build_graph(forward.clone());

    // 5. Meta: refresh/remove the changed files' records.
    let mut files = data.meta.files.clone();
    for rel in &changed_set {
        let abs = root.join(rel);
        if abs.exists() {
            let pkg = if matches!(Lang::of(Path::new(rel)), Lang::Markdown) {
                "docs".to_string()
            } else {
                ws.package_for(Path::new(rel)).map(|p| p.name.clone()).unwrap_or_else(|| "root".into())
            };
            files.insert(rel.clone(), FileRecord { mtime_ms: mtime_ms(&abs), pkg });
        } else {
            files.remove(rel);
        }
    }

    let bm25 = data.bm25;
    let mut meta = data.meta;
    meta.updated_at = now_millis();
    meta.files = files;

    Ok(IndexData { meta, symbols, chunks, vectors, forward, graph, bm25 })
}

/// Build a `PackageMeta`, inferring the package's role from its manifest deps at index time (so
/// retrieval weighting uses the real dependency signal, not a name/dir guess). `Unknown` → `None`.
fn package_meta(p: &ci_walk::Package) -> PackageMeta {
    let dir = p.dir.to_string_lossy().replace('\\', "/");
    let role = ci_core::weight::infer_role(&ci_core::weight::RoleSignals {
        name: p.name.clone(),
        dir: dir.clone(),
        deps: p.deps.clone(),
        ..Default::default()
    });
    let role = (role != ci_core::weight::PackageRole::Unknown).then(|| role.as_str().to_string());
    PackageMeta { name: p.name.clone(), dir, role }
}

/// The UNION of every active provider's import graph, as string adjacency (repo-relative posix
/// paths), dropping any file with no outgoing edges. Import edges are within-language and each
/// provider scopes its graph to its own files, so keys are disjoint across languages — combining
/// is a plain union. Both the full build and the incremental refresh take the whole graph from the
/// providers (SCIP is whole-project), so this is shared.
fn forward_adjacency(registry: &ProviderRegistry) -> Result<Adjacency> {
    let mut forward = Adjacency::new();
    for provider in registry.providers() {
        for (from, tos) in provider.import_graph()? {
            let from_s = from.to_string_lossy().replace('\\', "/");
            let tos_s: Vec<String> =
                tos.iter().map(|t| t.to_string_lossy().replace('\\', "/")).collect();
            if !tos_s.is_empty() {
                forward.insert(from_s, tos_s);
            }
        }
    }
    Ok(forward)
}

/// Walk a structure node, emitting an Item for every NAMED symbol (any depth).
fn node_items(node: &ci_core::Node, rel: &str, pkg: &str, content: &str, out: &mut Vec<Item>) {
    node.walk(&mut |n| {
        let Some(kind) = n.symbol_kind() else { return };
        let name = n.name.clone().unwrap_or_else(|| "?".into());
        let text = slice_lines(content, n.range.start_line, n.range.end_line);
        let sym = SymbolEntry {
            id: n.id.clone(),
            name: name.clone(),
            kind,
            file: rel.to_string(),
            pkg: pkg.to_string(),
            start_line: n.range.start_line,
            end_line: n.range.end_line,
            signature: None,
        };
        let chunk = ChunkMeta {
            id: n.id.clone(),
            symbol: name,
            kind,
            file: rel.to_string(),
            pkg: pkg.to_string(),
            start_line: n.range.start_line,
            end_line: n.range.end_line,
        };
        out.push(Item { sym, chunk, text });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_core::{Granularity, ImportGraph, LanguageProvider, Node, NodeKind, Range};
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// A stand-in provider: structure per relative path + a fixed import graph.
    struct MockProvider {
        by_file: HashMap<String, Vec<Node>>,
        graph: ImportGraph,
    }
    impl LanguageProvider for MockProvider {
        fn granularity(&self) -> Granularity {
            Granularity::Symbol
        }
        fn structure(&self, file: &Path) -> Result<Vec<Node>> {
            let rel = file.to_string_lossy().replace('\\', "/");
            Ok(self.by_file.get(&rel).cloned().unwrap_or_default())
        }
        fn import_graph(&self) -> Result<ImportGraph> {
            Ok(self.graph.clone())
        }
        fn apply_edits(
            &self,
            _ops: &[ci_core::EditOp],
            _opts: &ci_core::EditOpts,
        ) -> Result<ci_core::CommitResult> {
            unimplemented!("write path is P2")
        }
    }

    fn sym_node(id: &str, name: &str, sl: u32, el: u32) -> Node {
        Node {
            id: id.into(),
            name: Some(name.into()),
            kind: NodeKind::Symbol(SymbolKind::Function),
            range: Range { start_line: sl, end_line: el, start_char: 0, end_char: 0 },
            name_range: None,
            children: vec![],
        }
    }

    /// Trivial deterministic embedder (dim 4), so the test needs no model.
    fn toy_embed(text: &str) -> Vec<f32> {
        let mut v = [0f32; 4];
        for (i, b) in text.bytes().enumerate() {
            v[i % 4] += b as f32;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        v.iter().map(|x| x / norm).collect()
    }

    #[test]
    fn builds_index_from_provider() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/math.ts"), "export function add(a: number, b: number) {\n  return a + b;\n}\n").unwrap();
        fs::write(root.join("src/app.ts"), "import { add } from './math.js';\nexport function main() {\n  return add(1, 2);\n}\n").unwrap();

        let mut by_file = HashMap::new();
        by_file.insert("src/math.ts".into(), vec![sym_node("src/math.ts#add", "add", 1, 3)]);
        by_file.insert("src/app.ts".into(), vec![sym_node("src/app.ts#main", "main", 2, 4)]);
        let mut graph = ImportGraph::new();
        graph.insert(PathBuf::from("src/app.ts"), vec![PathBuf::from("src/math.ts")]);
        let provider = MockProvider { by_file, graph };

        let config = Config { index_docs: false, ..Default::default() };
        let registry = ProviderRegistry::single(Arc::new(provider));
        let index = build_index(root, &config, &registry, toy_embed).unwrap();

        // Two code symbols chunked; vectors row-aligned at dim 4.
        assert_eq!(index.chunks.len(), 2);
        assert_eq!(index.meta.dims, 4);
        assert_eq!(index.vectors.len(), 2 * 4);

        // Chunk text was sliced from the real file (add's body present).
        // BM25 finds `add` in math.ts's chunk.
        let hits = index.bm25.search(&tokenize("add"), 5);
        assert!(hits.iter().any(|(id, _)| id == "src/math.ts#add"));

        // Import graph: app -> math, reverse derived.
        assert_eq!(index.graph.reverse.get("src/math.ts").unwrap(), &vec!["src/app.ts".to_string()]);
    }

    #[test]
    fn dispatches_per_file_and_unions_graphs() {
        // A mixed TS+Rust repo: each file must be indexed by ITS language's provider, and the two
        // providers' import graphs must combine into one (union) graph.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/app.ts"), "import { add } from './math.js';\nexport function main() {}\n").unwrap();
        fs::write(root.join("src/math.ts"), "export function add() {}\n").unwrap();
        fs::write(root.join("src/lib.rs"), "mod util;\npub fn run() {}\n").unwrap();
        fs::write(root.join("src/util.rs"), "pub fn helper() {}\n").unwrap();

        // TS provider: only its files + its within-language graph (app.ts -> math.ts).
        let mut ts_files = HashMap::new();
        ts_files.insert("src/app.ts".into(), vec![sym_node("src/app.ts#main", "main", 2, 2)]);
        ts_files.insert("src/math.ts".into(), vec![sym_node("src/math.ts#add", "add", 1, 1)]);
        let mut ts_graph = ImportGraph::new();
        ts_graph.insert(PathBuf::from("src/app.ts"), vec![PathBuf::from("src/math.ts")]);
        let ts = MockProvider { by_file: ts_files, graph: ts_graph };

        // Rust provider: only its files + its own graph (lib.rs -> util.rs).
        let mut rs_files = HashMap::new();
        rs_files.insert("src/lib.rs".into(), vec![sym_node("src/lib.rs#run", "run", 2, 2)]);
        rs_files.insert("src/util.rs".into(), vec![sym_node("src/util.rs#helper", "helper", 1, 1)]);
        let mut rs_graph = ImportGraph::new();
        rs_graph.insert(PathBuf::from("src/lib.rs"), vec![PathBuf::from("src/util.rs")]);
        let rs = MockProvider { by_file: rs_files, graph: rs_graph };

        let mut registry = ProviderRegistry::new();
        registry.register(vec![Lang::Ts, Lang::Tsx], Arc::new(ts));
        registry.register(vec![Lang::Rust], Arc::new(rs));

        let config = Config { index_docs: false, include: vec!["**/*.ts".into(), "**/*.rs".into()], ..Default::default() };
        let index = build_index(root, &config, &registry, toy_embed).unwrap();

        // Every language's symbols are indexed into the one unified index.
        let ids: Vec<&str> = index.chunks.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"src/app.ts#main"), "TS symbol indexed");
        assert!(ids.contains(&"src/util.rs#helper"), "Rust symbol indexed");

        // The graph is the union of both providers' edges (disjoint file keys).
        assert_eq!(index.graph.reverse.get("src/math.ts").unwrap(), &vec!["src/app.ts".to_string()]);
        assert_eq!(index.graph.reverse.get("src/util.rs").unwrap(), &vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn update_index_refreshes_only_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.ts"), "export function alpha() {\n  return 1;\n}\n").unwrap();
        fs::write(root.join("src/b.ts"), "export function bee() {\n  return 0;\n}\n").unwrap();

        let mut by_file = HashMap::new();
        by_file.insert("src/a.ts".into(), vec![sym_node("src/a.ts#alpha", "alpha", 1, 3)]);
        by_file.insert("src/b.ts".into(), vec![sym_node("src/b.ts#bee", "bee", 1, 3)]);
        let v1 = MockProvider { by_file, graph: ImportGraph::new() };

        let config = Config { index_docs: false, ..Default::default() };
        let r1 = ProviderRegistry::single(Arc::new(v1));
        let initial = build_index(root, &config, &r1, toy_embed).unwrap();
        assert!(initial.bm25.search(&tokenize("alpha"), 5).iter().any(|(id, _)| id == "src/a.ts#alpha"));

        // Edit a.ts on disk + provider now reports the new structure.
        fs::write(root.join("src/a.ts"), "export function beta() {\n  return 2;\n}\n").unwrap();
        let mut by_file2 = HashMap::new();
        by_file2.insert("src/a.ts".into(), vec![sym_node("src/a.ts#beta", "beta", 1, 3)]);
        by_file2.insert("src/b.ts".into(), vec![sym_node("src/b.ts#bee", "bee", 1, 3)]);
        let v2 = MockProvider { by_file: by_file2, graph: ImportGraph::new() };

        let r2 = ProviderRegistry::single(Arc::new(v2));
        let updated = update_index(root, &r2, toy_embed, initial, &["src/a.ts".into()]).unwrap();

        // New symbol present, stale one gone, untouched file intact.
        assert!(updated.chunks.iter().any(|c| c.id == "src/a.ts#beta"));
        assert!(!updated.chunks.iter().any(|c| c.id == "src/a.ts#alpha"));
        assert!(updated.chunks.iter().any(|c| c.id == "src/b.ts#bee"));
        assert!(updated.bm25.search(&tokenize("beta"), 5).iter().any(|(id, _)| id == "src/a.ts#beta"));
        assert!(updated.bm25.search(&tokenize("alpha"), 5).is_empty());
        // vectors stay row-aligned with chunks.
        assert_eq!(updated.vectors.len(), updated.chunks.len() * updated.meta.dims);
    }

    #[test]
    fn package_role_inferred_from_manifest_deps() {
        // A crate that depends on axum → backend role persisted in PackageMeta (from Cargo deps).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = Config { index_docs: false, ..Default::default() };
        fs::create_dir_all(root.join("svc/src")).unwrap();
        fs::write(
            root.join("svc/Cargo.toml"),
            "[package]\nname = \"svc\"\n[dependencies]\naxum = \"0.7\"\n",
        )
        .unwrap();
        fs::write(root.join("svc/src/lib.rs"), "pub fn handler() {}\n").unwrap();

        let provider = MockProvider { by_file: HashMap::new(), graph: ImportGraph::new() };
        let registry = ProviderRegistry::single(Arc::new(provider));
        let index = build_index(root, &config, &registry, toy_embed).unwrap();
        let svc = index.meta.packages.iter().find(|p| p.name == "svc").expect("svc package");
        assert_eq!(svc.role.as_deref(), Some("backend"), "axum dep → backend role persisted");
    }

    #[test]
    fn update_then_persist_reload_reflects_the_edit() {
        // The Batch 2 loop's persist half: build → edit → update_index → SAVE → reload from disk.
        // The reloaded index must reflect the edit (new symbol in, stale out), proving the
        // post-edit reindex the MCP server now runs actually survives a round-trip.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = Config { index_docs: false, ..Default::default() };
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.ts"), "export function alpha() {\n  return 1;\n}\n").unwrap();

        let mut by_file = HashMap::new();
        by_file.insert("src/a.ts".into(), vec![sym_node("src/a.ts#alpha", "alpha", 1, 3)]);
        let v1 = MockProvider { by_file, graph: ImportGraph::new() };
        let r1 = ProviderRegistry::single(Arc::new(v1));
        let initial = build_index(root, &config, &r1, toy_embed).unwrap();
        ci_index::save_index(root, &config, &initial).unwrap();

        // Edit a.ts on disk + provider reports the new structure; incrementally update, then persist.
        fs::write(root.join("src/a.ts"), "export function beta() {\n  return 2;\n}\n").unwrap();
        let mut by_file2 = HashMap::new();
        by_file2.insert("src/a.ts".into(), vec![sym_node("src/a.ts#beta", "beta", 1, 3)]);
        let v2 = MockProvider { by_file: by_file2, graph: ImportGraph::new() };
        let r2 = ProviderRegistry::single(Arc::new(v2));
        let updated = update_index(root, &r2, toy_embed, initial, &["src/a.ts".into()]).unwrap();
        ci_index::save_index(root, &config, &updated).unwrap();

        // Reload from disk — the edit is durable.
        let reloaded = ci_index::load_index(root, &config).unwrap();
        assert!(reloaded.chunks.iter().any(|c| c.id == "src/a.ts#beta"), "new symbol persisted");
        assert!(!reloaded.chunks.iter().any(|c| c.id == "src/a.ts#alpha"), "stale symbol gone");
        assert_eq!(reloaded.vectors.len(), reloaded.chunks.len() * reloaded.meta.dims);
    }
}
