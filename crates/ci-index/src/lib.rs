//! ci-index — storage primitives for the retrieval index: BM25, a flat dense
//! vector store, the import graph, and protobuf+binary persistence. The build/update
//! pipeline (which calls a `CodeDriver` for symbols/imports) lands once a driver
//! exists.
pub mod bm25;
pub mod graph;
pub mod pb;
pub mod store;
pub mod types;
pub mod vector;

pub use bm25::{tokenize, Bm25, Bm25Doc, Bm25Json};
pub use graph::{build_graph, derive_reverse, Adjacency, GraphData};
pub use store::{index_dir, index_exists, load_index, save_index, IndexData, INDEX_VERSION};
pub use types::{ChunkMeta, FileRecord, IndexMeta, PackageMeta, SymbolEntry};
pub use vector::{cosine_normalized, rank_matrix};
