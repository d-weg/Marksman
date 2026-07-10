//! phpactor resolution + launch — the rename/willRename half of the PHP engine.
//!
//! Two facts drive this module (both spec-verified):
//! - phpactor implements `workspace/willRenameFiles` as REAL LSP fileOperations (its
//!   `FileRenameHandler` returns a WorkspaceEdit rewriting the class name/namespace +
//!   references), so a PHP move is engine-native through the standard LSP channel — no custom
//!   RPC. Intelephense is EXCLUDED: its rename (and the file-rename that rides on it) is
//!   premium-licensed.
//! - phpactor ships as a PHAR from GitHub releases (not composer global) and needs a PHP
//!   runtime with posix — so the launcher is `php phpactor.phar language-server`.
use ci_core::Result;
use ci_lsp::LspClient;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const INSTALL_HINT: &str =
    "the phpactor PHAR from https://github.com/phpactor/phpactor/releases (point $CI_PHPACTOR at it; \
     it runs on the `php` runtime)";

/// The phpactor PHAR path: `$CI_PHPACTOR`, else a `phpactor`/`phpactor.phar` on PATH, else the
/// Homebrew prefixes. `None` = rename/move falls back to the movefix hooks.
pub(crate) fn phpactor_phar() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CI_PHPACTOR") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in ["phpactor", "phpactor.phar"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    ["/opt/homebrew/bin/phpactor", "/usr/local/bin/phpactor"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Start phpactor's language server for `root`. A bare `phpactor` binary is launched directly;
/// a `.phar` is run through the `php` runtime (`php phpactor.phar language-server`).
pub(crate) fn start(root: &Path, sandbox: &dyn ci_core::Sandbox) -> Result<LspClient> {
    // The image exposes phpactor as a bare `phpactor` launcher on PATH; `tool_command` resolves it
    // by name there, else the host PHAR (its absence is what the container fixes).
    let mut cmd = ci_core::tool_command(sandbox, "phpactor", || {
        let Some(phar) = phpactor_phar() else {
            return Err(ci_core::Error::Driver(format!(
                "php rename/move needs phpactor to rewrite references safely — Install: {INSTALL_HINT}. \
                 Without it, reissue a SYMBOL rename as `replace_text` edits over the definition and \
                 each reference in one batch — the phpstan gate type-checks the result, so a missed or \
                 wrong site rejects rather than lands."
            )));
        };
        // A `.phar` runs through the `php` runtime; a bare binary launches directly.
        Ok(if phar.extension().and_then(|e| e.to_str()) == Some("phar") {
            let mut c = Command::new("php");
            c.arg(&phar);
            c
        } else {
            Command::new(&phar)
        })
    })?;
    cmd.arg("language-server").current_dir(root);
    LspClient::start_in(root, cmd, sandbox)
}
