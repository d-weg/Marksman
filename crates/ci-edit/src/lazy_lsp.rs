//! A lazily-started [`LspClient`] slot — the LSP-rewrite half every "batch verdict + LSP
//! rename" engine repeats (java/jdtls, php/phpactor, swift/sourcekit). Diagnostics never wait
//! on the LSP, and a missing rename tool costs nothing until an op actually needs cross-file
//! rewrites — so the client starts on FIRST USE, and the disk-sync/fs-event notifications a
//! never-started client would need are no-ops.
use crate::GateEngine;
use ci_core::Result;
use ci_lsp::LspClient;
use serde_json::Value;

/// A rename/willRename LSP that starts on first use. Engines hold one and delegate their
/// `rename`/`sync_disk`/`fs_events`/`will_rename` plumbing here instead of hand-rolling the
/// `Option<LspClient>` dance.
pub struct LazyLsp {
    slot: Option<LspClient>,
    start: Box<dyn FnMut() -> Result<LspClient> + Send>,
}

impl LazyLsp {
    /// `start` launches the client (a `jdtls::start`-shaped closure capturing root + sandbox);
    /// it runs at most once — the started client is reused for the engine's lifetime.
    pub fn new(start: impl FnMut() -> Result<LspClient> + Send + 'static) -> Self {
        Self { slot: None, start: Box::new(start) }
    }

    /// The client, started on first use.
    pub fn get(&mut self) -> Result<&mut LspClient> {
        if self.slot.is_none() {
            self.slot = Some((self.start)()?);
        }
        Ok(self.slot.as_mut().expect("just set"))
    }

    /// Resync a STARTED client's buffers with disk; a never-started client has no state to sync.
    pub fn sync_disk(&mut self) -> Result<()> {
        match self.slot.as_mut() {
            Some(lsp) => lsp.sync_disk(),
            None => Ok(()),
        }
    }

    /// Forward file create/delete events to a STARTED client; no-op otherwise.
    pub fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        match self.slot.as_mut() {
            Some(lsp) => lsp.fs_events(created, deleted),
            None => Ok(()),
        }
    }

    /// Contract §8 ordering for a file move: the LSP's `willRenameFiles` when the client starts
    /// AND yields a non-empty WorkspaceEdit; otherwise `fallback` (the syntactic movefix hooks).
    /// A failed start or a declined/empty answer falls through SILENTLY — the fallback is the
    /// runnable rewrite either way, and the type-check gate judges whichever rewrite lands.
    pub fn will_rename_or(
        &mut self,
        from: &str,
        to: &str,
        fallback: impl FnOnce() -> Value,
    ) -> Result<Value> {
        if let Ok(lsp) = self.get() {
            if let Ok(we) = GateEngine::will_rename(lsp, from, to) {
                if !crate::workspace_edit_is_empty(&we) {
                    return Ok(we);
                }
            }
        }
        Ok(fallback())
    }
}
