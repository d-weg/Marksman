//! Native-Rust Model2Vec static embedder. Faithful port of the Node
//! `StaticEmbedder` (src/static-embedder.ts), itself a port of model2vec's
//! quantized `StaticModel.encode`:
//!
//!   ids      = tokenize(text, add_special_tokens=false), drop unk, cap max_length
//!   emb[i]   = embedding[mapping[id_i]] * weights[id_i]   (weights by ORIGINAL id)
//!   vec      = mean_i(emb[i]);  then vec /= (||vec|| + 1e-32) when normalize
//!
//! Accumulation is f64, embedding rows are f32, weights f64 — matching the Node
//! port so the parity test reproduces the Python reference bit-for-bit.
use ci_core::{Error, Result};
use safetensors::SafeTensors;
use std::path::Path;
use tokenizers::Tokenizer;

pub struct StaticEmbedder {
    embedding: Vec<f32>, // [vocab * dim], row-major
    weights: Vec<f64>,   // [vocab], indexed by original token id
    mapping: Vec<i64>,   // [vocab], token id -> embedding row
    dim: usize,
    normalize: bool,
    unk_token_id: u32,
    max_length: usize,
    median_token_length: usize,
    tokenizer: Tokenizer,
}

impl StaticEmbedder {
    pub fn load(model_dir: &Path) -> Result<Self> {
        let bytes = std::fs::read(model_dir.join("model.safetensors"))?;
        let st = SafeTensors::deserialize(&bytes)
            .map_err(|e| Error::Other(format!("safetensors: {e}")))?;

        let emb = st.tensor("embeddings").map_err(|e| Error::Other(format!("embeddings: {e}")))?;
        let w = st.tensor("weights").map_err(|e| Error::Other(format!("weights: {e}")))?;
        let map = st.tensor("mapping").map_err(|e| Error::Other(format!("mapping: {e}")))?;

        let dim = *emb.shape().get(1).ok_or_else(|| Error::Other("embeddings not 2-D".into()))?;
        let embedding = bytes_to_f32(emb.data());
        let weights = bytes_to_f64(w.data());
        let mapping = bytes_to_i64(map.data());

        let normalize = read_normalize(model_dir).unwrap_or(true);
        let tokenizer = Tokenizer::from_file(model_dir.join("tokenizer.json"))
            .map_err(|e| Error::Other(format!("tokenizer: {e}")))?;

        Ok(Self {
            embedding,
            weights,
            mapping,
            dim,
            normalize,
            unk_token_id: 1,
            max_length: 512,
            median_token_length: 7,
            tokenizer,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Token ids matching model2vec.tokenize: no specials, drop unk, cap length.
    pub fn tokenize(&self, text: &str) -> Result<Vec<u32>> {
        let cap = self.max_length * self.median_token_length;
        let sliced: String = text.chars().take(cap).collect();
        let input = if sliced.is_empty() { " ".to_string() } else { sliced };
        let enc = self
            .tokenizer
            .encode(input, false)
            .map_err(|e| Error::Other(format!("encode: {e}")))?;
        Ok(enc
            .get_ids()
            .iter()
            .copied()
            .filter(|&id| id != self.unk_token_id)
            .take(self.max_length)
            .collect())
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let ids = self.tokenize(text)?;
        let mut acc = vec![0f64; self.dim];
        if ids.is_empty() {
            return Ok(vec![0f32; self.dim]);
        }
        for &id in &ids {
            let idu = id as usize;
            if idu >= self.mapping.len() {
                continue;
            }
            let row = self.mapping[idu] as usize;
            let w = self.weights[idu];
            let base = row * self.dim;
            for d in 0..self.dim {
                acc[d] += self.embedding[base + d] as f64 * w;
            }
        }
        let inv = 1.0 / ids.len() as f64;
        for v in acc.iter_mut() {
            *v *= inv;
        }
        if self.normalize {
            let mut norm = 0f64;
            for v in &acc {
                norm += v * v;
            }
            norm = norm.sqrt() + 1e-32;
            for v in acc.iter_mut() {
                *v /= norm;
            }
        }
        Ok(acc.into_iter().map(|x| x as f32).collect())
    }
}

fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
fn bytes_to_f64(b: &[u8]) -> Vec<f64> {
    b.chunks_exact(8)
        .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}
fn bytes_to_i64(b: &[u8]) -> Vec<i64> {
    b.chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

fn read_normalize(dir: &Path) -> Option<bool> {
    let raw = std::fs::read_to_string(dir.join("config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("normalize")?.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The old TS repo holds the model + the Python reference vectors. Layout:
    /// /Users/.../codeindex (old)  and  /Users/.../codeindex-rs (this).
    fn old_repo() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../codeindex")
    }

    fn cosine(a: &[f32], b: &[f64]) -> f64 {
        let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
        for i in 0..a.len() {
            let x = a[i] as f64;
            dot += x * b[i];
            na += x * x;
            nb += b[i] * b[i];
        }
        dot / (na.sqrt() * nb.sqrt())
    }

    #[test]
    fn parity_with_python_reference() {
        let repo = old_repo();
        let model_dir = repo.join(".models/potion-code-16M");
        let parity = repo.join("scripts/bench-embedder/.data/parity.json");
        if !model_dir.exists() || !parity.exists() {
            eprintln!("SKIP parity: assets missing under {}", repo.display());
            return;
        }
        let fx: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&parity).unwrap()).unwrap();
        let emb = StaticEmbedder::load(&model_dir).unwrap();
        assert_eq!(emb.dim(), fx["dims"].as_u64().unwrap() as usize);

        let samples = fx["samples"].as_array().unwrap();
        let token_ids = fx["tokenIds"].as_array().unwrap();
        let vectors = fx["vectors"].as_array().unwrap();

        let mut worst = 1f64;
        for i in 0..samples.len() {
            let text = samples[i].as_str().unwrap();
            let ids = emb.tokenize(text).unwrap();
            let ref_ids: Vec<u32> = token_ids[i]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u32)
                .collect();
            assert_eq!(ids, ref_ids, "token ids differ on sample {i}");

            let vec = emb.embed(text).unwrap();
            let refv: Vec<f64> = vectors[i]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap())
                .collect();
            worst = worst.min(cosine(&vec, &refv));
        }
        eprintln!("parity: worst cosine = {worst:.8} over {} samples", samples.len());
        assert!(worst > 0.99999, "worst cosine {worst} below threshold");
    }
}
