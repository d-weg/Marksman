//! sourcekit-lsp resolution + launch — the rename half of the Swift engine.
//!
//! Facts driving this module (all spec-verified, Swift section):
//! - sourcekit-lsp SHIPS WITH THE TOOLCHAIN (Xcode / Swift.org) — no separate install. Rename is
//!   production-quality since the Swift 6 toolchain: cross-file, `prepareRename`, served from the
//!   IndexStoreDB global index with background indexing ON BY DEFAULT since 6.1. Rename
//!   correctness = index freshness (the same staleness class the Rust gate already handles).
//! - It is **SwiftPM-only** — no Xcode-project support. The toolchain probe says so; a non-SwiftPM
//!   repo gets no rename engine (the §8 hooks still cover moves).
//! - `willRenameFiles` is REFUTED (no handler exists), so moves never go through the LSP — the §8
//!   move hooks own them (see `movefix.rs`).
//! - Pull diagnostics (LSP 3.17) are registered DYNAMICALLY; a client must advertise
//!   `textDocument.diagnostic.dynamicRegistration: true` and handle `workspace/diagnostic/refresh`
//!   or it silently degrades to publish mode. We do NOT use sourcekit-lsp for the gate verdict
//!   (that is `swift build`, the sound gate), so this degradation cannot produce a false-clean
//!   here — the LSP serves ONLY rename. The shared ci-lsp client advertises the capability anyway
//!   (honest capability reporting), but Swift's soundness never depends on it.
use ci_core::Result;
use ci_lsp::LspClient;
use std::path::PathBuf;
use std::process::Command;

pub(crate) const INSTALL_HINT: &str =
    "sourcekit-lsp ships with the Swift toolchain (Xcode command-line tools or the Swift.org \
     toolchain / `swiftly`); point $CI_SOURCEKIT_LSP at it if it isn't on PATH";

/// The sourcekit-lsp binary: `$CI_SOURCEKIT_LSP`, else `sourcekit-lsp` on PATH, else the common
/// toolchain locations. `None` = rename is unavailable (a rename op then explains itself).
pub(crate) fn sourcekit_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CI_SOURCEKIT_LSP") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("sourcekit-lsp");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    [
        "/usr/bin/sourcekit-lsp",
        "/usr/local/bin/sourcekit-lsp",
        "/Library/Developer/CommandLineTools/usr/bin/sourcekit-lsp",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

/// Start sourcekit-lsp for `root`. Launched directly (it discovers the SwiftPM package from the
/// workspace root); background indexing warms the IndexStoreDB the rename reads from.
pub(crate) fn start(root: &std::path::Path) -> Result<LspClient> {
    let Some(bin) = sourcekit_binary() else {
        return Err(ci_core::Error::Driver(format!(
            "swift rename needs sourcekit-lsp — Install: {INSTALL_HINT}"
        )));
    };
    let mut cmd = Command::new(bin);
    cmd.current_dir(root);
    LspClient::start(root, cmd)
}
