//! `peashooter-provider-swift` — the Swift language provider as a standalone sidecar process
//! (the `CI_PROVIDER=sidecar` wire; `lang-rust`'s sidecar is the pattern).
//! Usage: `peashooter-provider-swift --root /path/to/repo` (then it serves stdin/stdout).
use lang_swift::SwiftProvider;
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

    // The registry builders gate on `swift` before spawning this process; a bare launch deserves
    // the same honesty instead of a spawn error on the first edit.
    if let Some(missing) = lang_swift::gate_missing() {
        eprintln!("[peashooter-provider-swift] {missing}");
        std::process::exit(1);
    }
    let provider = SwiftProvider::new(&root);
    let outline = |content: &str| lang_fallback::outline(lang_fallback::FbLang::Swift, content);
    if let Err(e) = ci_proto::serve_stdio(provider, outline) {
        eprintln!("[peashooter-provider-swift] serve error: {e}");
        std::process::exit(1);
    }
}
