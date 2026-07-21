//! `peashooter-provider-java` — the Java language provider as a standalone sidecar process
//! (the `CI_PROVIDER=sidecar` wire; `lang-rust`'s sidecar is the pattern).
//! Usage: `peashooter-provider-java --root /path/to/repo` (then it serves stdin/stdout).
use lang_java::JavaProvider;
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

    // The registry builders gate on javac before spawning this process; a bare launch
    // deserves the same honesty instead of a spawn error on the first edit.
    if let Some(missing) = lang_java::gate_missing() {
        eprintln!("[peashooter-provider-java] {missing}");
        std::process::exit(1);
    }
    let provider = JavaProvider::new(&root);
    let outline = |content: &str| lang_fallback::outline(lang_fallback::FbLang::Java, content);
    if let Err(e) = ci_proto::serve_stdio(provider, outline) {
        eprintln!("[peashooter-provider-java] serve error: {e}");
        std::process::exit(1);
    }
}
