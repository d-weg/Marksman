//! ci-retrieve — RRF fusion + import-graph expansion + package weighting into a
//! context Manifest. Language-blind: operates on a loaded IndexData + an injected
//! query embedding.
pub mod retrieve;
pub mod rrf;

pub use retrieve::{retrieve, RetrieveOptions};
pub use rrf::{reciprocal_rank_fusion, sorted_by_score};
