//! ci-embed — native Model2Vec static embeddings (no Python, no ONNX).
//!
//! v1 supports Model2Vec static models only (e.g. minishlab/potion-code-16M).
//! Transformer models (bge via ONNX) are future work — they'd need an `ort`
//! backend; the static path is what the project actually uses.
mod static_embedder;

pub use static_embedder::StaticEmbedder;

/// True for Model2Vec static models (mirrors `isStaticModel` in the TS tool).
pub fn is_static_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("potion") || m.contains("model2vec") || m.starts_with("minishlab/")
}
