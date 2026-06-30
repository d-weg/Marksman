//! `marksman-provider-rust` — the Rust language provider as a standalone sidecar process.
//!
//! The core spawns this and talks to it over the [`ci_proto`] protobuf wire instead of linking
//! `RustProvider` in-process — the first step toward downloadable, language-agnostic providers.
//! Usage: `marksman-provider-rust --root /path/to/repo` (then it serves stdin/stdout).
use lang_rust::{outline, RustProvider};
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

    let provider = RustProvider::new(&root);
    if let Err(e) = ci_proto::serve_stdio(provider, outline) {
        eprintln!("[marksman-provider-rust] serve error: {e}");
        std::process::exit(1);
    }
}
