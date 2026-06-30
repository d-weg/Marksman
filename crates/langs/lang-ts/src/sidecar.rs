//! `marksman-provider-ts` — the TypeScript language provider as a standalone sidecar process.
//!
//! Runs `scip-typescript` to index `--root`, then serves `TsProvider` over the [`ci_proto`]
//! protobuf wire (the ts-morph / LSP gate runs inside this process, so `apply_edits` is served
//! here too). Usage: `marksman-provider-ts --root /path/to/repo`.
use lang_ts::{outline, TsProvider};
use std::path::PathBuf;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let root = argv
        .iter()
        .position(|a| a == "--root")
        .and_then(|i| argv.get(i + 1))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();

    let provider = match TsProvider::index(&root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[marksman-provider-ts] indexing {} failed: {e}", root.display());
            std::process::exit(1);
        }
    };
    provider.prewarm();
    if let Err(e) = ci_proto::serve_stdio(provider, outline) {
        eprintln!("[marksman-provider-ts] serve error: {e}");
        std::process::exit(1);
    }
}
