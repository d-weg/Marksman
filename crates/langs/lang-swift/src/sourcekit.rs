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
    ci_core::discover_tool(
        "CI_SOURCEKIT_LSP",
        &["sourcekit-lsp"],
        &[
            "/usr/bin/sourcekit-lsp",
            "/usr/local/bin/sourcekit-lsp",
            "/Library/Developer/CommandLineTools/usr/bin/sourcekit-lsp",
        ],
    )
}

/// Start sourcekit-lsp for `root`. Launched directly (it discovers the SwiftPM package from the
/// workspace root); background indexing warms the IndexStoreDB the rename reads from.
pub(crate) fn start(root: &std::path::Path, sandbox: &dyn ci_core::Sandbox) -> Result<LspClient> {
    // The image ships sourcekit-lsp on PATH; `tool_command` resolves it by bare name there, else the
    // host binary.
    let mut cmd = ci_core::tool_command(sandbox, "sourcekit-lsp", || {
        let Some(bin) = sourcekit_binary() else {
            return Err(ci_core::Error::Driver(format!(
                "swift rename needs sourcekit-lsp — Install: {INSTALL_HINT}"
            )));
        };
        Ok(Command::new(bin))
    })?;
    // Cross-file rename reads the IndexStoreDB. In a CONTAINER there is no prior indexed build, so
    // sourcekit must build the index itself (`background-indexing`) and the rename must wait for it
    // (ensure_ready via `set_expects_index_progress`) — otherwise it rewrites only the definition. A
    // host has an existing/fast index and already renames cross-file, so it pays neither (host path
    // unchanged).
    let containerized = sandbox.containerized();
    if containerized {
        cmd.arg("--experimental-feature").arg("background-indexing");
    }
    cmd.current_dir(root);
    let mut client = LspClient::start_in(root, cmd, sandbox)?;
    if containerized {
        client.set_expects_index_progress();
    }
    Ok(client)
}
