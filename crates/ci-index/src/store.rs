//! Persistence — mirrors src/store.ts: JSON sidecars + raw little-endian f32 for
//! the embedding matrix, under `<root>/<indexDir>`.
use crate::bm25::Bm25;
use crate::graph::{build_graph, Adjacency, GraphData};
use crate::types::{ChunkMeta, IndexMeta, SymbolEntry};
use ci_core::{Config, Result};
use std::path::{Path, PathBuf};

pub struct IndexData {
    pub meta: IndexMeta,
    pub symbols: Vec<SymbolEntry>,
    pub chunks: Vec<ChunkMeta>,
    /// chunks.len() * meta.dims, row-aligned with `chunks`.
    pub vectors: Vec<f32>,
    pub forward: Adjacency,
    pub graph: GraphData,
    pub bm25: Bm25,
}

pub fn index_dir(root: &Path, config: &Config) -> PathBuf {
    root.join(&config.index_dir)
}

pub fn index_exists(root: &Path, config: &Config) -> bool {
    index_dir(root, config).join("meta.json").exists()
}

pub fn save_index(root: &Path, config: &Config, data: &IndexData) -> Result<()> {
    let dir = index_dir(root, config);
    std::fs::create_dir_all(&dir)?;
    write_json(&dir.join("meta.json"), &data.meta)?;
    write_json(&dir.join("symbols.json"), &data.symbols)?;
    write_json(&dir.join("chunks.json"), &data.chunks)?;
    write_json(&dir.join("graph.json"), &data.forward)?;
    write_json(&dir.join("bm25.json"), &data.bm25.to_json())?;

    let mut buf = Vec::with_capacity(data.vectors.len() * 4);
    for v in &data.vectors {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(dir.join("embeddings.bin"), buf)?;
    write_json(&dir.join("config.snapshot.json"), config)?;
    Ok(())
}

pub fn load_index(root: &Path, config: &Config) -> Result<IndexData> {
    let dir = index_dir(root, config);
    let meta: IndexMeta = read_json(&dir.join("meta.json"))?;
    let symbols: Vec<SymbolEntry> = read_json(&dir.join("symbols.json"))?;
    let chunks: Vec<ChunkMeta> = read_json(&dir.join("chunks.json"))?;
    let forward: Adjacency = read_json(&dir.join("graph.json"))?;
    let bm25 = Bm25::from_json(read_json(&dir.join("bm25.json"))?);

    let raw = std::fs::read(dir.join("embeddings.bin"))?;
    let vectors: Vec<f32> = raw
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    let graph = build_graph(forward.clone());
    Ok(IndexData { meta, symbols, chunks, vectors, forward, graph, bm25 })
}

fn write_json<T: serde::Serialize>(p: &Path, v: &T) -> Result<()> {
    std::fs::write(p, serde_json::to_vec(v)?)?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&std::fs::read(p)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bm25::tokenize;
    use crate::types::*;
    use std::collections::BTreeMap;

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = Config::default();

        let mut bm = Bm25::new();
        bm.add_doc("a.ts#x@1", "a.ts", &tokenize("alpha beta"));
        let mut forward = Adjacency::new();
        forward.insert("a.ts".into(), vec!["b.ts".into()]);

        let data = IndexData {
            meta: IndexMeta {
                version: 1,
                created_at: "t".into(),
                updated_at: "t".into(),
                model: "m".into(),
                dims: 2,
                root: root.display().to_string(),
                is_monorepo: false,
                packages: vec![],
                package_names: vec![],
                files: BTreeMap::new(),
            },
            symbols: vec![],
            chunks: vec![ChunkMeta {
                id: "a.ts#x@1".into(),
                symbol: "x".into(),
                kind: ci_core::SymbolKind::Function,
                file: "a.ts".into(),
                pkg: "root".into(),
                start_line: 1,
                end_line: 2,
            }],
            vectors: vec![0.1, 0.2, 0.3, 0.4],
            forward: forward.clone(),
            graph: build_graph(forward),
            bm25: bm,
        };

        save_index(root, &config, &data).unwrap();
        assert!(index_exists(root, &config));

        let loaded = load_index(root, &config).unwrap();
        assert_eq!(loaded.chunks.len(), 1);
        assert_eq!(loaded.vectors, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(loaded.graph.reverse.get("b.ts").unwrap(), &vec!["a.ts".to_string()]);
        assert_eq!(loaded.bm25.search(&tokenize("beta"), 5)[0].0, "a.ts#x@1");
    }
}
