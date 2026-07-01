//! Retrieval orchestration — port of src/retrieve.ts. Query embedding is INJECTED
//! (`query_vec`) so this crate stays free of the embedder and unit-testable; the
//! CLI/MCP layer embeds via ci-embed.
use crate::rrf::{reciprocal_rank_fusion, sorted_by_score};
use ci_core::weight::{
    infer_role_from_path, layer_multipliers, resolve_role, PackageRole, WeightedPackage,
};
use ci_core::{Config, Manifest, ManifestEntry, MatchedSym, SeedRank, SymbolKind};
use ci_index::{rank_matrix, tokenize, ChunkMeta, GraphData, IndexData, SymbolEntry};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct RetrieveOptions {
    pub top_n: Option<usize>,
    pub hops: Option<usize>,
    pub max_expand: Option<usize>,
}

fn is_doc_file(f: &str) -> bool {
    f.ends_with(".md") || f.ends_with(".mdx")
}

fn files_from_rows(rows: &[(usize, f64)], chunks: &[ChunkMeta]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (row, _) in rows {
        if let Some(c) = chunks.get(*row) {
            if seen.insert(c.file.clone()) {
                out.push(c.file.clone());
            }
        }
    }
    out
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Does `needle` occur in `hay` as a whole identifier — flanked by non-identifier chars (or
/// string edges)? Prevents a short symbol like `name` from "exactly" matching inside `rename`,
/// which is what made common field names hijack the symbol-match bonus. Both args lowercased.
fn contains_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hb = hay.as_bytes();
    let mut from = 0;
    while let Some(off) = hay[from..].find(needle) {
        let i = from + off;
        let end = i + needle.len();
        let before_ok = i == 0 || !is_ident_char(hb[i - 1]);
        let after_ok = end >= hb.len() || !is_ident_char(hb[end]);
        if before_ok && after_ok {
            return true;
        }
        // Advance by one whole char (i is a char boundary; i+1 may split a multibyte
        // codepoint and panic the next slice on non-ASCII identifiers).
        from = i + hay[i..].chars().next().map_or(1, |c| c.len_utf8());
    }
    false
}

/// Per-file direct symbol-name match. Returns the best-scoring file first, each tagged with its
/// match `score` and whether it was an `exact` full-name hit (the query literally contains the
/// symbol's whole name — a near-certain "this is the symbol I mean"). The caller uses `score` to
/// size a relevance bonus and `exact` to force the defining file in as a seed.
fn symbol_name_search(
    symbols: &[SymbolEntry],
    q_tokens: &[String],
    q_raw: &str,
) -> Vec<(String, i32, bool)> {
    let q: HashSet<&str> = q_tokens.iter().map(String::as_str).collect();
    let ql = q_raw.to_lowercase();
    let mut best: HashMap<String, (i32, bool)> = HashMap::new();
    for s in symbols {
        if matches!(s.kind, SymbolKind::Doc) {
            continue;
        }
        let mut score = 0i32;
        for t in tokenize(&s.name) {
            if q.contains(t.as_str()) {
                score += 1;
            }
        }
        let exact = s.name.chars().count() >= 3 && contains_word(&ql, &s.name.to_lowercase());
        if exact {
            score += 2;
        }
        if score <= 0 {
            continue;
        }
        let e = best.entry(s.file.clone()).or_insert((0, false));
        if score > e.0 {
            // Keep a prior exact hit even when a higher-scoring NON-exact symbol in the same
            // file arrives later — otherwise the exact definition loses its bonus/forced-seed.
            *e = (score, exact || e.1);
        } else if exact {
            e.1 = true;
        }
    }
    let mut v: Vec<(String, i32, bool)> =
        best.into_iter().map(|(f, (sc, ex))| (f, sc, ex)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

#[derive(Clone)]
struct Exp {
    reason: &'static str,
    #[allow(dead_code)]
    hop: usize,
}

/// Expand seeds 1..hops along the import graph, both directions, tagging the relation.
fn expand_graph(seeds: &[String], graph: &GraphData, hops: usize) -> HashMap<String, Exp> {
    let mut result: HashMap<String, Exp> = HashMap::new();
    let mut visited: HashSet<String> = seeds.iter().cloned().collect();
    let mut frontier: HashSet<String> = seeds.iter().cloned().collect();
    for h in 1..=hops {
        let mut next = HashSet::new();
        for f in &frontier {
            if let Some(imps) = graph.forward.get(f) {
                for imp in imps {
                    if visited.insert(imp.clone()) {
                        next.insert(imp.clone());
                        result.entry(imp.clone()).or_insert(Exp { reason: "imported-by-seed", hop: h });
                    }
                }
            }
            if let Some(revs) = graph.reverse.get(f) {
                for rev in revs {
                    if visited.insert(rev.clone()) {
                        next.insert(rev.clone());
                        result.entry(rev.clone()).or_insert(Exp { reason: "imports-seed", hop: h });
                    }
                }
            }
        }
        frontier = next;
    }
    result
}

/// How many seeds a file is import-adjacent to (1 hop, either direction).
fn adjacency_to_seeds(file: &str, seeds: &HashSet<String>, graph: &GraphData) -> usize {
    let empty: Vec<String> = Vec::new();
    let fwd: HashSet<&String> = graph.forward.get(file).unwrap_or(&empty).iter().collect();
    let rev: HashSet<&String> = graph.reverse.get(file).unwrap_or(&empty).iter().collect();
    let mut c = 0;
    for s in seeds {
        if s == file {
            continue;
        }
        if fwd.contains(s) || rev.contains(s) {
            c += 1;
        }
    }
    c
}

fn now_millis() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        .to_string()
}

/// Produce a context manifest for `task` against a loaded index, given the query
/// embedding. Mirrors retrieve.ts step-for-step.
pub fn retrieve(
    root: &Path,
    task: &str,
    index: &IndexData,
    query_vec: &[f32],
    config: &Config,
    opts: &RetrieveOptions,
) -> Manifest {
    let top_n = opts.top_n.unwrap_or(config.top_n);
    let hops = opts.hops.unwrap_or(config.graph_hops);
    let max_expand = opts.max_expand.unwrap_or(config.max_expand);
    let dims = index.meta.dims;

    // 1. Three searches.
    let vec_rows = rank_matrix(&index.vectors, dims, query_vec, 80);
    let vec_files = files_from_rows(&vec_rows, &index.chunks);

    let q_tokens = tokenize(task);
    let bm_hits = index.bm25.search(&q_tokens, 80);
    let id_to_chunk: HashMap<&str, &ChunkMeta> =
        index.chunks.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut bm_files = Vec::new();
    let mut seen_bm = HashSet::new();
    for (id, _) in &bm_hits {
        if let Some(c) = id_to_chunk.get(id.as_str()) {
            if seen_bm.insert(c.file.clone()) {
                bm_files.push(c.file.clone());
            }
        }
    }

    let sym_scored = symbol_name_search(&index.symbols, &q_tokens, task);
    let sym_files: Vec<String> = sym_scored.iter().map(|(f, _, _)| f.clone()).collect();
    // The bonus is reserved for the "you named it exactly" case (query contains the symbol's full
    // name) — that file is almost certainly the target. Partial-token matches already get their
    // due through RRF's `sym_files` list; spreading the bonus to them just floats every loosely
    // related file and re-buries the definition. So strength is binary: 1.0 for an exact hit.
    let sym_strength: HashMap<String, f64> = sym_scored
        .iter()
        .filter(|(_, _, ex)| *ex)
        .map(|(f, _, _)| (f.clone(), 1.0))
        .collect();
    let exact_sym_files: Vec<String> =
        sym_scored.iter().filter(|(_, _, ex)| *ex).map(|(f, _, _)| f.clone()).take(5).collect();

    // 2. RRF.
    let fused = reciprocal_rank_fusion(&[vec_files, bm_files, sym_files], config.rrf_k as f64);

    // 3. Package- AND path-aware weighting (post-RRF multiply on the fused score).
    // Static multiplier stays at package granularity; the query-conditioned LAYER boost keys off
    // the file's *path*-derived role (falling back to the package's role), so a `backend`/`db`
    // directory is boosted on a backend query even inside a single-package repo. See weight.rs.
    let weight_for = file_weighter(index, &q_tokens, config);

    let mut weighted_fused: HashMap<String, f64> = HashMap::new();
    for (file, s) in &fused {
        weighted_fused.insert(file.clone(), s * weight_for(file));
    }
    let fused_sorted = sorted_by_score(&weighted_fused);

    // 4. Seeds + graph expansion. Files the query names exactly are forced in as seeds even if
    // RRF consensus + adjacency would otherwise bury the (small, leaf) definition site.
    let mut seeds: Vec<String> = fused_sorted.iter().take(top_n).map(|(f, _)| f.clone()).collect();
    for f in &exact_sym_files {
        if !seeds.contains(f) {
            seeds.push(f.clone());
        }
    }
    let seed_set: HashSet<String> = seeds.iter().cloned().collect();
    let expanded = expand_graph(&seeds, &index.graph, hops);

    // Matched symbols from the top vector rows.
    let mut matched: HashMap<String, Vec<MatchedSym>> = HashMap::new();
    for (row, _) in vec_rows.iter().take(40) {
        if let Some(c) = index.chunks.get(*row) {
            if !matches!(c.kind, SymbolKind::Doc) {
                let arr = matched.entry(c.file.clone()).or_default();
                if arr.len() < 6
                    && !arr.iter().any(|x| x.name == c.symbol && x.line_range[0] == c.start_line)
                {
                    arr.push(MatchedSym {
                        node_id: c.id.clone(),
                        name: c.symbol.clone(),
                        kind: c.kind,
                        line_range: [c.start_line, c.end_line],
                    });
                }
            }
        }
    }

    // 5. Scoring: weighted-fused + adjacency bonus.
    let mut candidates: HashSet<String> = HashSet::new();
    candidates.extend(seeds.iter().cloned());
    candidates.extend(expanded.keys().cloned());

    let mut seed_entries: Vec<ManifestEntry> = Vec::new();
    let mut exp_entries: Vec<ManifestEntry> = Vec::new();
    for file in &candidates {
        let is_seed = seed_set.contains(file);
        let exp = expanded.get(file);
        if !is_seed && exp.is_none() {
            continue;
        }
        let base = *weighted_fused.get(file).unwrap_or(&0.0);
        let adj = adjacency_to_seeds(file, &seed_set, &index.graph);
        // Symbol-match bonus: lifts the file that *defines* the named symbol above hub files that
        // only score via adjacency. Scaled by match strength (1.0 = the query named it in full).
        let sym_b = config.symbol_match_bonus as f64 * sym_strength.get(file).copied().unwrap_or(0.0);
        let score = base + config.adjacency_bonus as f64 * adj as f64 + sym_b;
        let reason: String = if is_seed {
            if is_doc_file(file) { "doc".into() } else { "query-match".into() }
        } else {
            exp.unwrap().reason.to_string()
        };
        let ms = matched.get(file).cloned().unwrap_or_default();
        let pkg = index
            .meta
            .files
            .get(file)
            .map(|f| f.pkg.clone())
            .or_else(|| index.chunks.iter().find(|c| &c.file == file).map(|c| c.pkg.clone()))
            .unwrap_or_else(|| "root".into());
        let mut entry = ManifestEntry {
            file: file.clone(),
            pkg,
            matched_symbols: ms,
            reason,
            score,
            whole_file: None,
        };
        if let Ok(content) = std::fs::read_to_string(root.join(file)) {
            if content.split('\n').count() <= 50 {
                entry.whole_file = Some(true);
            }
        }
        if is_seed {
            seed_entries.push(entry);
        } else {
            exp_entries.push(entry);
        }
    }

    exp_entries
        .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut entries = seed_entries;
    entries.extend(exp_entries.into_iter().take(max_expand));
    entries.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    let seed_ranking: Vec<SeedRank> = fused_sorted
        .iter()
        .take(top_n)
        .map(|(f, s)| SeedRank { file: f.clone(), score: *s })
        .collect();

    Manifest {
        task: task.to_string(),
        generated_at: now_millis(),
        root: root.display().to_string(),
        entries,
        seed_ranking,
    }
}

/// A per-file relevance multiplier: static package weight × the query-conditioned layer boost, keyed
/// on the file's path-derived role (falling back to its package role). Shared by `retrieve` (post-
/// RRF multiply) and `find_symbols` (result ranking). Borrows `index`/`config` for its lifetime.
pub fn file_weighter<'a>(
    index: &'a IndexData,
    query_tokens: &[String],
    config: &'a Config,
) -> impl Fn(&str) -> f64 + 'a {
    let packages: Vec<WeightedPackage> = index
        .meta
        .packages
        .iter()
        .map(|p| WeightedPackage { name: p.name.clone(), dir: p.dir.clone(), ..Default::default() })
        .collect();
    let pkg_role: HashMap<String, PackageRole> =
        packages.iter().map(|p| (p.name.clone(), resolve_role(p))).collect();
    let layer_mult = layer_multipliers(query_tokens, config);
    move |file: &str| -> f64 {
        let pkg = index.meta.files.get(file).map(|f| f.pkg.as_str()).unwrap_or("");
        let prole = *pkg_role.get(pkg).unwrap_or(&PackageRole::Unknown);
        // path role wins when decisive; otherwise the package's role.
        let frole = match infer_role_from_path(file) {
            PackageRole::Unknown => prole,
            r => r,
        };
        let static_w = config
            .package_weights
            .get(pkg)
            .or_else(|| config.package_weights.get(prole.as_str()))
            .map(|w| *w as f64)
            .unwrap_or(1.0);
        static_w * layer_mult.get(frole.as_str()).copied().unwrap_or(1.0)
    }
}

/// One `find_symbols` hit: a self-locating node-id **handle** (feed it straight to `read_node` /
/// `apply_edits`), its kind, 1-based line range, whether the name matched exactly, and file weight.
#[derive(Debug, Clone)]
pub struct SymbolHit {
    pub node_id: String,
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    pub line_range: [u32; 2],
    pub exact: bool,
    pub weight: f64,
}

/// Exhaustive keyword/symbol search over the index — the handle-returning counterpart to
/// `retrieve_context`. Matches indexed symbol names (exact, or substring when `substring`), skips
/// docs, and ranks exact-first, then by path-role/layer weight, then id. Returns up to `cap` hits
/// plus the TOTAL match count (so the caller can note truncation). Deterministic.
pub fn find_symbols(
    index: &IndexData,
    query: &str,
    substring: bool,
    config: &Config,
    cap: usize,
) -> (Vec<SymbolHit>, usize) {
    let ql = query.trim().to_lowercase();
    if ql.is_empty() {
        return (Vec::new(), 0);
    }
    let weight_for = file_weighter(index, &tokenize(query), config);
    let mut hits: Vec<SymbolHit> = index
        .symbols
        .iter()
        .filter(|s| !matches!(s.kind, SymbolKind::Doc))
        .filter_map(|s| {
            let nl = s.name.to_lowercase();
            let exact = nl == ql;
            if !(if substring { nl.contains(&ql) } else { exact }) {
                return None;
            }
            Some(SymbolHit {
                node_id: s.id.clone(),
                name: s.name.clone(),
                kind: s.kind,
                file: s.file.clone(),
                line_range: [s.start_line, s.end_line],
                exact,
                weight: weight_for(&s.file),
            })
        })
        .collect();
    let total = hits.len();
    hits.sort_by(|a, b| {
        b.exact
            .cmp(&a.exact)
            .then(b.weight.partial_cmp(&a.weight).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.node_id.cmp(&b.node_id))
    });
    hits.truncate(cap);
    (hits, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_index::{build_graph, Adjacency, Bm25, ChunkMeta, IndexData, IndexMeta, SymbolEntry};
    use ci_index::FileRecord;
    use std::collections::BTreeMap;

    fn chunk(id: &str, sym: &str, file: &str, sl: u32, el: u32) -> ChunkMeta {
        ChunkMeta {
            id: id.into(),
            symbol: sym.into(),
            kind: SymbolKind::Function,
            file: file.into(),
            pkg: "root".into(),
            start_line: sl,
            end_line: el,
        }
    }

    #[test]
    fn contains_word_respects_identifier_boundaries() {
        // the bug: `name` must NOT match inside `rename`.
        assert!(!contains_word("rename the function", "name"));
        assert!(contains_word("rename the reciprocalrankfusion function", "reciprocalrankfusion"));
        assert!(contains_word("the name field", "name")); // standalone -> matches
        assert!(!contains_word("my_name helper", "name")); // snake_case part is not a whole word
        // multibyte: a non-ASCII identifier must not panic the byte-advance, and the
        // standalone occurrence still matches (the first, inside "xóy", does not).
        assert!(contains_word("xóy ó", "ó"));
        assert!(!contains_word("xóy", "ó"));
    }

    fn sym(file: &str, name: &str) -> SymbolEntry {
        SymbolEntry {
            id: format!("{file}#{name}"),
            name: name.into(),
            kind: SymbolKind::Function,
            file: file.into(),
            pkg: "root".into(),
            start_line: 1,
            end_line: 2,
            signature: None,
        }
    }

    fn index_with_symbols(symbols: Vec<SymbolEntry>) -> IndexData {
        IndexData {
            meta: IndexMeta {
                version: 1, created_at: "0".into(), updated_at: "0".into(), model: "m".into(),
                dims: 2, root: "/tmp".into(), is_monorepo: false, packages: vec![],
                package_names: vec![], files: BTreeMap::new(),
            },
            symbols,
            chunks: vec![],
            vectors: vec![],
            forward: Adjacency::new(),
            graph: build_graph(Adjacency::new()),
            bm25: Bm25::new(),
        }
    }

    #[test]
    fn find_symbols_returns_handles_exact_first_excludes_docs() {
        let index = index_with_symbols(vec![
            sym("a.ts", "parseConfig"),
            sym("b.ts", "parse"),
            sym("c.ts", "parseConfigFile"),
            SymbolEntry { kind: SymbolKind::Doc, ..sym("d.md", "parse") }, // doc: never a hit
        ]);

        // Exact mode: only the whole-name "parse" (doc excluded).
        let (hits, total) = find_symbols(&index, "parse", false, &Config::default(), 50);
        assert_eq!(total, 1, "exact-only match count");
        assert_eq!(hits[0].node_id, "b.ts#parse", "hit is a self-locating handle");
        assert!(hits[0].exact);

        // Substring mode: three code symbols contain "parse"; the exact one ranks first.
        let (hits, total) = find_symbols(&index, "parse", true, &Config::default(), 50);
        assert_eq!(total, 3);
        assert_eq!(hits[0].node_id, "b.ts#parse", "exact match ranks ahead of substrings");
        assert!(hits.iter().all(|h| h.kind != SymbolKind::Doc), "docs excluded");

        // Cap truncates but the total still reports every match.
        let (capped, total) = find_symbols(&index, "parse", true, &Config::default(), 1);
        assert_eq!((capped.len(), total), (1, 3));
    }

    #[test]
    fn exact_flag_survives_higher_scoring_nonexact_symbol() {
        // F defines an exact-named symbol (Foo, score 3) AND a higher-scoring non-exact one
        // (barBazQuxLib, 4 token hits) listed AFTER it. The exact bit must not be overwritten.
        let raw = "rename Foo bar baz qux lib";
        let q = tokenize(raw);
        let syms = vec![sym("F", "Foo"), sym("F", "barBazQuxLib")];
        let res = symbol_name_search(&syms, &q, raw);
        let f = res.iter().find(|(file, _, _)| file == "F").expect("F present");
        assert!(f.2, "exact flag must survive a later higher-scoring non-exact symbol");
    }

    #[test]
    fn exact_symbol_match_outranks_adjacency_hub() {
        // leaf.ts DEFINES reciprocalRankFusion; hub.ts is imported by many seeds (pure adjacency).
        let chunks = vec![
            chunk("leaf.ts#rrf@1", "reciprocalRankFusion", "leaf.ts", 1, 10),
            chunk("hub.ts#types@1", "Types", "hub.ts", 1, 5),
            chunk("a.ts#a@1", "a", "a.ts", 1, 5),
            chunk("b.ts#b@1", "b", "b.ts", 1, 5),
        ];
        let vectors = vec![0.0, 1.0, 1.0, 0.0, 0.9, 0.1, 0.1, 0.9];
        let mut bm = Bm25::new();
        bm.add_doc("a.ts#a@1", "a.ts", &tokenize("alpha helper"));
        bm.add_doc("b.ts#b@1", "b.ts", &tokenize("beta helper"));
        let symbols = vec![SymbolEntry {
            id: "leaf.ts#rrf@1".into(),
            name: "reciprocalRankFusion".into(),
            kind: SymbolKind::Function,
            file: "leaf.ts".into(),
            pkg: "root".into(),
            start_line: 1,
            end_line: 10,
            signature: None,
        }];
        // a.ts and b.ts (seeds) both import hub.ts -> hub.ts is a 2-adjacency hub.
        let mut forward = Adjacency::new();
        forward.insert("a.ts".into(), vec!["hub.ts".into()]);
        forward.insert("b.ts".into(), vec!["hub.ts".into()]);
        let mut files = BTreeMap::new();
        for f in ["leaf.ts", "hub.ts", "a.ts", "b.ts"] {
            files.insert(f.to_string(), FileRecord { mtime_ms: 0.0, pkg: "root".into() });
        }
        let index = IndexData {
            meta: IndexMeta {
                version: 1, created_at: "0".into(), updated_at: "0".into(), model: "m".into(),
                dims: 2, root: "/tmp".into(), is_monorepo: false, packages: vec![],
                package_names: vec![], files,
            },
            symbols, chunks, vectors,
            forward: forward.clone(), graph: build_graph(forward), bm25: bm,
        };
        let manifest = retrieve(
            Path::new("/nonexistent"),
            "rename the reciprocalRankFusion function",
            &index,
            &[0.0, 1.0],
            &Config::default(),
            &RetrieveOptions { top_n: Some(2), ..Default::default() },
        );
        // the exact-named definition must be the #1 entry, ahead of the adjacency hub.
        assert_eq!(manifest.entries[0].file, "leaf.ts", "definition should rank first: {:?}",
            manifest.entries.iter().map(|e| (&e.file, e.score)).collect::<Vec<_>>());
    }

    #[test]
    fn retrieves_seed_and_expands_graph() {
        // a.ts holds reciprocalRankFusion; b.ts imports a.ts; c.ts unrelated.
        let chunks = vec![
            chunk("a.ts#rrf@1", "reciprocalRankFusion", "a.ts", 1, 10),
            chunk("b.ts#caller@1", "caller", "b.ts", 1, 5),
            chunk("c.ts#misc@1", "misc", "c.ts", 1, 5),
        ];
        // dim 2 normalized-ish vectors: a near [1,0], b near [0,1], c near [0.7,0.7].
        let vectors = vec![1.0, 0.0, 0.0, 1.0, 0.7, 0.7];

        let mut bm = Bm25::new();
        bm.add_doc("a.ts#rrf@1", "a.ts", &tokenize("reciprocal rank fusion merge"));
        bm.add_doc("b.ts#caller@1", "b.ts", &tokenize("calls fusion helper"));
        bm.add_doc("c.ts#misc@1", "c.ts", &tokenize("database migration unrelated"));

        let symbols = vec![SymbolEntry {
            id: "a.ts#rrf@1".into(),
            name: "reciprocalRankFusion".into(),
            kind: SymbolKind::Function,
            file: "a.ts".into(),
            pkg: "root".into(),
            start_line: 1,
            end_line: 10,
            signature: None,
        }];

        let mut forward = Adjacency::new();
        forward.insert("b.ts".into(), vec!["a.ts".into()]); // b imports a

        let mut files = BTreeMap::new();
        for f in ["a.ts", "b.ts", "c.ts"] {
            files.insert(f.to_string(), FileRecord { mtime_ms: 0.0, pkg: "root".into() });
        }

        let index = IndexData {
            meta: IndexMeta {
                version: 1,
                created_at: "0".into(),
                updated_at: "0".into(),
                model: "m".into(),
                dims: 2,
                root: "/tmp".into(),
                is_monorepo: false,
                packages: vec![],
                package_names: vec![],
                files,
            },
            symbols,
            chunks,
            vectors,
            forward: forward.clone(),
            graph: build_graph(forward),
            bm25: bm,
        };

        // top_n=1 so only a.ts seeds; b.ts must arrive via graph expansion.
        let manifest = retrieve(
            Path::new("/nonexistent"),
            "reciprocal rank fusion",
            &index,
            &[1.0, 0.0],
            &Config::default(),
            &RetrieveOptions { top_n: Some(1), ..Default::default() },
        );

        // a.ts is the lone seed (top_n=1) -> query-match.
        let a = manifest.entries.iter().find(|e| e.file == "a.ts").expect("a.ts present");
        assert_eq!(a.reason, "query-match");
        // b.ts imports a.ts -> surfaces via graph expansion as imports-seed.
        let b = manifest.entries.iter().find(|e| e.file == "b.ts").expect("b.ts present");
        assert_eq!(b.reason, "imports-seed");
        // c.ts is unrelated and not import-adjacent -> excluded entirely.
        assert!(manifest.entries.iter().all(|e| e.file != "c.ts"), "c.ts should not appear");
    }
}
