//! Protobuf on-disk form of the retrieval index (`index.pb`): prost messages mirrored from the
//! in-memory types, hand-written like ci-proto's wire messages — no protoc, no build step.
//! Binary over JSON because the store is parsed on every cold start and grows linearly with the
//! repo. The embedding matrix stays in `embeddings.bin` (raw little-endian f32 — already the
//! fastest possible form). Any decode failure is a hard error ("re-run index"), never a silent
//! partial read.
use crate::bm25::{Bm25Doc, Bm25Json};
use crate::graph::Adjacency;
use crate::store::IndexData;
use crate::types::{ChunkMeta, FileRecord, IndexMeta, PackageMeta, SymbolEntry};
use ci_core::{Error, Result, SymbolKind};
use prost::Message;
use std::collections::HashMap;

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbIndex {
    #[prost(message, optional, tag = "1")]
    pub meta: Option<PbMeta>,
    #[prost(message, repeated, tag = "2")]
    pub symbols: Vec<PbSymbol>,
    #[prost(message, repeated, tag = "3")]
    pub chunks: Vec<PbChunk>,
    #[prost(message, repeated, tag = "4")]
    pub forward: Vec<PbEdge>,
    #[prost(message, optional, tag = "5")]
    pub bm25: Option<PbBm25>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbMeta {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(string, tag = "2")]
    pub created_at: String,
    #[prost(string, tag = "3")]
    pub updated_at: String,
    #[prost(string, tag = "4")]
    pub model: String,
    #[prost(uint64, tag = "5")]
    pub dims: u64,
    #[prost(string, tag = "6")]
    pub root: String,
    #[prost(bool, tag = "7")]
    pub is_monorepo: bool,
    #[prost(message, repeated, tag = "8")]
    pub packages: Vec<PbPackage>,
    #[prost(string, repeated, tag = "9")]
    pub package_names: Vec<String>,
    #[prost(message, repeated, tag = "10")]
    pub files: Vec<PbFile>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbPackage {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub dir: String,
    #[prost(string, optional, tag = "3")]
    pub role: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbFile {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(double, tag = "2")]
    pub mtime_ms: f64,
    #[prost(string, tag = "3")]
    pub pkg: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbSymbol {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(int32, tag = "3")]
    pub kind: i32,
    #[prost(string, tag = "4")]
    pub file: String,
    #[prost(string, tag = "5")]
    pub pkg: String,
    #[prost(uint32, tag = "6")]
    pub start_line: u32,
    #[prost(uint32, tag = "7")]
    pub end_line: u32,
    #[prost(string, optional, tag = "8")]
    pub signature: Option<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbChunk {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub symbol: String,
    #[prost(int32, tag = "3")]
    pub kind: i32,
    #[prost(string, tag = "4")]
    pub file: String,
    #[prost(string, tag = "5")]
    pub pkg: String,
    #[prost(uint32, tag = "6")]
    pub start_line: u32,
    #[prost(uint32, tag = "7")]
    pub end_line: u32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbEdge {
    #[prost(string, tag = "1")]
    pub from: String,
    #[prost(string, repeated, tag = "2")]
    pub to: Vec<String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbBm25 {
    #[prost(double, tag = "1")]
    pub k1: f64,
    #[prost(double, tag = "2")]
    pub b: f64,
    #[prost(message, repeated, tag = "3")]
    pub docs: Vec<PbBm25Doc>,
    #[prost(map = "string, uint64", tag = "4")]
    pub df: HashMap<String, u64>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PbBm25Doc {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub file: String,
    #[prost(uint64, tag = "3")]
    pub len: u64,
    #[prost(map = "string, uint32", tag = "4")]
    pub tf: HashMap<String, u32>,
}

/// SymbolKind wire values — additions get NEW numbers; never reuse one (an old index must
/// decode as an error, not as a different kind).
fn kind_to_i32(k: SymbolKind) -> i32 {
    match k {
        SymbolKind::Function => 0,
        SymbolKind::Class => 1,
        SymbolKind::Interface => 2,
        SymbolKind::Enum => 3,
        SymbolKind::TypeAlias => 4,
        SymbolKind::Variable => 5,
        SymbolKind::Method => 6,
        SymbolKind::Struct => 7,
    }
}

fn kind_from_i32(v: i32) -> Result<SymbolKind> {
    Ok(match v {
        0 => SymbolKind::Function,
        1 => SymbolKind::Class,
        2 => SymbolKind::Interface,
        3 => SymbolKind::Enum,
        4 => SymbolKind::TypeAlias,
        5 => SymbolKind::Variable,
        6 => SymbolKind::Method,
        7 => SymbolKind::Struct,
        other => return Err(Error::Other(format!("index.pb: unknown symbol kind {other} — re-run `index`"))),
    })
}

/// Serialize everything except the embedding matrix into the `index.pb` bytes.
pub(crate) fn encode_index(data: &IndexData) -> Vec<u8> {
    let bm = data.bm25.to_json();
    let pb = PbIndex {
        meta: Some(PbMeta {
            version: data.meta.version,
            created_at: data.meta.created_at.clone(),
            updated_at: data.meta.updated_at.clone(),
            model: data.meta.model.clone(),
            dims: data.meta.dims as u64,
            root: data.meta.root.clone(),
            is_monorepo: data.meta.is_monorepo,
            packages: data
                .meta
                .packages
                .iter()
                .map(|p| PbPackage { name: p.name.clone(), dir: p.dir.clone(), role: p.role.clone() })
                .collect(),
            package_names: data.meta.package_names.clone(),
            files: data
                .meta
                .files
                .iter()
                .map(|(path, f)| PbFile { path: path.clone(), mtime_ms: f.mtime_ms, pkg: f.pkg.clone() })
                .collect(),
        }),
        symbols: data
            .symbols
            .iter()
            .map(|s| PbSymbol {
                id: s.id.clone(),
                name: s.name.clone(),
                kind: kind_to_i32(s.kind),
                file: s.file.clone(),
                pkg: s.pkg.clone(),
                start_line: s.start_line,
                end_line: s.end_line,
                signature: s.signature.clone(),
            })
            .collect(),
        chunks: data
            .chunks
            .iter()
            .map(|c| PbChunk {
                id: c.id.clone(),
                symbol: c.symbol.clone(),
                kind: kind_to_i32(c.kind),
                file: c.file.clone(),
                pkg: c.pkg.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
            })
            .collect(),
        forward: data.forward.iter().map(|(from, to)| PbEdge { from: from.clone(), to: to.clone() }).collect(),
        bm25: Some(PbBm25 {
            k1: bm.k1,
            b: bm.b,
            docs: bm
                .docs
                .into_iter()
                .map(|d| PbBm25Doc { id: d.id, file: d.file, len: d.len as u64, tf: d.tf })
                .collect(),
            df: bm.df.into_iter().map(|(k, v)| (k, v as u64)).collect(),
        }),
    };
    pb.encode_to_vec()
}

/// The store's decoded parts (vectors/graph are rebuilt by the caller).
pub(crate) type DecodedIndex = (IndexMeta, Vec<SymbolEntry>, Vec<ChunkMeta>, Adjacency, Bm25Json);

/// Decode `index.pb` bytes back into the store's parts.
pub(crate) fn decode_index(bytes: &[u8]) -> Result<DecodedIndex> {
    let pb = PbIndex::decode(bytes).map_err(|e| Error::Other(format!("index.pb decode: {e} — re-run `index`")))?;
    let m = pb.meta.ok_or_else(|| Error::Other("index.pb has no meta — re-run `index`".into()))?;
    let meta = IndexMeta {
        version: m.version,
        created_at: m.created_at,
        updated_at: m.updated_at,
        model: m.model,
        dims: m.dims as usize,
        root: m.root,
        is_monorepo: m.is_monorepo,
        packages: m.packages.into_iter().map(|p| PackageMeta { name: p.name, dir: p.dir, role: p.role }).collect(),
        package_names: m.package_names,
        files: m.files.into_iter().map(|f| (f.path, FileRecord { mtime_ms: f.mtime_ms, pkg: f.pkg })).collect(),
    };
    let symbols = pb
        .symbols
        .into_iter()
        .map(|s| {
            Ok(SymbolEntry {
                id: s.id,
                name: s.name,
                kind: kind_from_i32(s.kind)?,
                file: s.file,
                pkg: s.pkg,
                start_line: s.start_line,
                end_line: s.end_line,
                signature: s.signature,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let chunks = pb
        .chunks
        .into_iter()
        .map(|c| {
            Ok(ChunkMeta {
                id: c.id,
                symbol: c.symbol,
                kind: kind_from_i32(c.kind)?,
                file: c.file,
                pkg: c.pkg,
                start_line: c.start_line,
                end_line: c.end_line,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let forward: Adjacency = pb.forward.into_iter().map(|e| (e.from, e.to)).collect();
    let bm = pb.bm25.unwrap_or_default();
    let bm25 = Bm25Json {
        k1: bm.k1,
        b: bm.b,
        docs: bm
            .docs
            .into_iter()
            .map(|d| Bm25Doc { id: d.id, file: d.file, len: d.len as usize, tf: d.tf })
            .collect(),
        df: bm.df.into_iter().map(|(k, v)| (k, v as usize)).collect(),
    };
    Ok((meta, symbols, chunks, forward, bm25))
}
