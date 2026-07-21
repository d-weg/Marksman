//! `peashooter-provider-php` — the PHP language provider as a standalone sidecar process
//! (the `CI_PROVIDER=sidecar` wire; `lang-rust`/`lang-java`'s sidecar is the pattern).
//! Usage: `peashooter-provider-php --root /path/to/repo` (then it serves stdin/stdout).
use lang_php::PhpProvider;
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

    // The registry builders gate on php/phpstan before spawning this process; a bare launch
    // deserves the same honesty instead of an error on the first edit.
    if let Some(missing) = lang_php::gate_missing(&root) {
        eprintln!("[peashooter-provider-php] {missing}");
        std::process::exit(1);
    }
    let provider = PhpProvider::new(&root);
    let outline = |content: &str| lang_fallback::outline(lang_fallback::FbLang::Php, content);
    if let Err(e) = ci_proto::serve_stdio(provider, outline) {
        eprintln!("[peashooter-provider-php] serve error: {e}");
        std::process::exit(1);
    }
}
