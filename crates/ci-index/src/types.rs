//! Index-internal data model (richer than the driver's `SymbolAnchor`). Mirrors
//! the persisted shapes in src/types.ts.
use ci_core::SymbolKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolEntry {
    /// Stable id `${file}#${name}@${startLine}`.
    pub id: String,
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    pub pkg: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// One embedding/BM25 chunk, row-aligned with the embedding matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkMeta {
    pub id: String,
    pub symbol: String,
    pub kind: SymbolKind,
    pub file: String,
    pub pkg: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageMeta {
    pub name: String,
    pub dir: String,
    /// Role inferred from the manifest deps at index time (`backend`/`frontend`/…), so retrieval
    /// weighting uses the real dependency signal rather than re-guessing from name/dir at query time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    pub mtime_ms: f64,
    pub pkg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexMeta {
    pub version: u32,
    pub created_at: String,
    pub updated_at: String,
    pub model: String,
    pub dims: usize,
    pub root: String,
    pub is_monorepo: bool,
    pub packages: Vec<PackageMeta>,
    pub package_names: Vec<String>,
    pub files: BTreeMap<String, FileRecord>,
}
