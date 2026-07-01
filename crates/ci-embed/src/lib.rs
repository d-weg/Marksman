//! ci-embed — native Model2Vec static embeddings (no Python, no ONNX).
//!
//! v1 supports Model2Vec static models only (e.g. minishlab/potion-code-16M).
//! Transformer models (bge via ONNX) are future work — they'd need an `ort`
//! backend; the static path is what the project actually uses.
mod static_embedder;

use std::path::Path;

pub use static_embedder::StaticEmbedder;

/// True for Model2Vec static models (mirrors `isStaticModel` in the TS tool).
pub fn is_static_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("potion") || m.contains("model2vec") || m.starts_with("minishlab/")
}

/// Ensure the Model2Vec model exists under `dir`, fetching it from HuggingFace on first use — the
/// same lazy-tooling model as the language providers (index only what the repo needs). No-op when
/// already present. On a failed/absent fetch (offline, no `curl`, `CI_NO_MODEL_FETCH`) returns a
/// precise, actionable error with the manual command instead of a raw file-not-found.
pub fn ensure_model(dir: &Path, model_id: &str) -> Result<(), String> {
    if dir.join("model.safetensors").is_file() {
        return Ok(());
    }
    if std::env::var("CI_NO_MODEL_FETCH").is_err() {
        if let Err(e) = fetch_model(dir, model_id) {
            eprintln!("[ci-embed] auto-fetch of {model_id} failed: {e}");
        }
        if dir.join("model.safetensors").is_file() {
            return Ok(());
        }
    }
    let d = dir.display();
    Err(format!(
        "embedding model {model_id:?} not found at {d} (and it wasn't fetched). Get it with:\n  \
         mkdir -p \"{d}\" && curl -fL --output-dir \"{d}\" -O \
         https://huggingface.co/{model_id}/resolve/main/model.safetensors -O \
         https://huggingface.co/{model_id}/resolve/main/tokenizer.json -O \
         https://huggingface.co/{model_id}/resolve/main/config.json\n\
         …or set CI_MODEL_DIR to an existing model directory."
    ))
}

/// Best-effort download of the three model files via `curl` (no HTTP-client dependency; `curl`
/// ships on macOS and modern Windows/Linux). `config.json` is optional — a 404 there is fine.
fn fetch_model(dir: &Path, model_id: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let base = format!("https://huggingface.co/{model_id}/resolve/main");
    for file in ["model.safetensors", "tokenizer.json", "config.json"] {
        let out = dir.join(file);
        let status = std::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "600", "--output-dir"])
            .arg(dir)
            .arg("-O")
            .arg(format!("{base}/{file}"))
            .status()
            .map_err(|e| format!("launching curl: {e}"))?;
        if !status.success() {
            let _ = std::fs::remove_file(&out); // drop any partial/error body
            if file == "config.json" {
                continue; // optional
            }
            return Err(format!("curl {base}/{file} failed ({status})"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_model_is_noop_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("model.safetensors"), b"stub").unwrap();
        assert!(ensure_model(dir.path(), "minishlab/potion-code-16M").is_ok());
    }

    #[test]
    fn ensure_model_errors_with_hint_when_absent_and_fetch_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CI_NO_MODEL_FETCH", "1");
        let err = ensure_model(dir.path(), "minishlab/potion-code-16M").unwrap_err();
        std::env::remove_var("CI_NO_MODEL_FETCH");
        assert!(err.contains("CI_MODEL_DIR"), "error should give an actionable hint: {err}");
        assert!(err.contains("huggingface.co"), "error should include the fetch URL: {err}");
    }
}
