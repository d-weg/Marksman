//! Persistence — mirrors src/store.ts: JSON sidecars + raw little-endian f32 for
//! the embedding matrix, under `<root>/<indexDir>`.
use crate::bm25::Bm25;
use crate::graph::{build_graph, Adjacency, GraphData};
use crate::types::{ChunkMeta, IndexMeta, SymbolEntry};
use ci_core::{Config, Error, Result};
use std::path::{Path, PathBuf};

/// On-disk index schema version. Bump when a persisted shape changes; `load_index` refuses a
/// mismatched index (with a "re-run index" hint) rather than silently mis-reading an old layout.
/// v2: dropped the `doc` symbol kind — Marksman is code-only, so an old index that holds doc
/// chunks is rejected and rebuilt cleanly.
pub const INDEX_VERSION: u32 = 2;

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
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("index");

    // Single-writer lock: a CLI `index` racing the server's post-edit reindex would otherwise
    // interleave. Held for the whole write; released on drop (incl. early return).
    let _lock = IndexLock::acquire(parent, name)?;

    // Serialize the whole index into a sibling temp dir, then atomically rename it into place, so a
    // reader never sees a half-written index (e.g. new chunks against an old, misaligned matrix).
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    write_index_files(&tmp, config, data)?;

    // Swap: move any current index aside, move the new one in, then drop the old. `rename` is
    // atomic on one filesystem; the only window is a brief "index missing" (readers retry), never
    // a torn read. On failure the previous index is restored.
    let stale = parent.join(format!(".{name}.old-{}", std::process::id()));
    if dir.exists() {
        std::fs::rename(&dir, &stale)?;
    }
    match std::fs::rename(&tmp, &dir) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&stale);
            Ok(())
        }
        Err(e) => {
            if stale.exists() {
                let _ = std::fs::rename(&stale, &dir);
            }
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e.into())
        }
    }
}

/// Write every index sidecar into `dir` (used against a temp dir before the atomic swap).
fn write_index_files(dir: &Path, config: &Config, data: &IndexData) -> Result<()> {
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

/// A best-effort, self-healing single-writer lock for the index dir (a `.<name>.lock` file next to
/// it). Advisory: a lock older than `STALE_LOCK` is assumed left by a crashed writer and stolen;
/// otherwise a second writer gets a clear "in progress" error. Released on drop.
struct IndexLock {
    path: PathBuf,
}

impl IndexLock {
    const STALE: std::time::Duration = std::time::Duration::from_secs(900); // 15 min

    fn acquire(parent: &Path, name: &str) -> Result<Self> {
        let path = parent.join(format!(".{name}.lock"));
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(IndexLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Self::STALE);
                    if stale {
                        let _ = std::fs::remove_file(&path); // steal the abandoned lock, retry once
                        continue;
                    }
                    return Err(Error::Other(format!(
                        "another index write is in progress ({}); retry shortly",
                        path.display()
                    )));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Other("could not acquire index lock".into()))
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn load_index(root: &Path, config: &Config) -> Result<IndexData> {
    let dir = index_dir(root, config);
    let meta: IndexMeta = read_json(&dir.join("meta.json"))?;
    if meta.version != INDEX_VERSION {
        return Err(Error::Other(format!(
            "index schema v{} is not supported (this build expects v{INDEX_VERSION}) — re-run `index`",
            meta.version
        )));
    }
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
                version: INDEX_VERSION,
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

    fn tiny_index(root: &Path) -> IndexData {
        let mut forward = Adjacency::new();
        forward.insert("a.ts".into(), vec!["b.ts".into()]);
        IndexData {
            meta: IndexMeta {
                version: INDEX_VERSION, created_at: "t".into(), updated_at: "t".into(), model: "m".into(),
                dims: 2, root: root.display().to_string(), is_monorepo: false, packages: vec![],
                package_names: vec![], files: BTreeMap::new(),
            },
            symbols: vec![],
            chunks: vec![ChunkMeta {
                id: "a.ts#x@1".into(), symbol: "x".into(), kind: ci_core::SymbolKind::Function,
                file: "a.ts".into(), pkg: "root".into(), start_line: 1, end_line: 2,
            }],
            vectors: vec![0.1, 0.2],
            forward: forward.clone(),
            graph: build_graph(forward),
            bm25: Bm25::new(),
        }
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = Config::default();
        // Save twice so the overwrite/swap path runs, then assert a clean dir with no leftovers.
        save_index(root, &config, &tiny_index(root)).unwrap();
        save_index(root, &config, &tiny_index(root)).unwrap();

        let idx = index_dir(root, &config);
        let parent = idx.parent().unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-") || n.contains(".old-") || n.ends_with(".lock"))
            .collect();
        assert!(leftovers.is_empty(), "temp/lock artifacts left behind: {leftovers:?}");
        assert!(load_index(root, &config).is_ok(), "index still loads after atomic overwrite");
    }

    #[test]
    fn load_refuses_mismatched_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = Config::default();
        let mut data = tiny_index(root);
        data.meta.version = INDEX_VERSION + 1; // a future/incompatible schema
        save_index(root, &config, &data).unwrap();

        let err = match load_index(root, &config) {
            Ok(_) => panic!("expected a schema-version error, but load succeeded"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("schema"), "expected a schema-version error, got: {err}");
    }

    #[test]
    fn index_lock_is_exclusive_then_released() {
        let dir = tempfile::tempdir().unwrap();
        let l1 = IndexLock::acquire(dir.path(), ".codeindex-rs").expect("first lock acquires");
        assert!(
            IndexLock::acquire(dir.path(), ".codeindex-rs").is_err(),
            "a second writer is blocked while the first holds the lock"
        );
        drop(l1);
        assert!(
            IndexLock::acquire(dir.path(), ".codeindex-rs").is_ok(),
            "lock is released on drop"
        );
    }
}
