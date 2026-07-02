//! lang-template — the Step-1 provider skeleton, kept compiling so it can't rot.
//!
//! This is the [rollout ladder](../../../docs/benchmarks.md)'s Step 1, as a crate you copy:
//! **tree-sitter reads + your language's real checker as the edit gate**. It is the shape the
//! ablation proved (−36% vs baseline before any SCIP indexer exists) and the shape
//! `TsTreeGated` ships in production for the `CI_TS_MODE=treesitter-gated` mode. A SCIP read
//! path is Step 2 — added later, when the language's users hit monorepos / cross-package blast
//! radius. Don't block a language on it.
//!
//! To add a gated language (`docs/provider-contract.md` has the full checklist):
//!
//! 1. **Reads**: make sure the language is in `lang_fallback::FbLang` (grammar dependency +
//!    `classify` table rows — see any existing language there). That gives you `structure()`,
//!    the syntactic import graph, outlines, and the `:body`/`:params`/`:doc` anchors.
//! 2. **Gate**: replace [`GatedTreeSitter::engine_factory`]'s payload with your checker.
//!    The generic path is `ci_lsp::LspClient::start(root, cmd)` — it implements
//!    [`GateEngine`] and prefers LSP 3.17 pull diagnostics (request/response; a slow server
//!    can never be mistaken for a clean file). Example for a pyright-gated Python:
//!    `Command::new("pyright-langserver"); cmd.arg("--stdio")`.
//! 3. **Wiring**: a `LangSpec` in `ci-build`'s registry (extensions, ignore dirs), a
//!    `make_provider` arm in `ci-mcp`, and a `doctor` toolchain probe with an actionable
//!    install hint — a missing checker DISABLES the language with instructions
//!    (`ProviderBuild::Unavailable`), it never silently falls back to the ungated tier.
//! 4. **Proof**: fixtures in `ci-conformance/tests/conformance.rs` (the fast battery), plus an
//!    `#[ignore]` e2e against the real checker in your crate: reject a type error, accept a
//!    clean edit, land a cross-file rename (`treesitter_gated_gates_and_renames` in lang-ts is
//!    the pattern). A `marksman-provider-<lang>` sidecar bin comes with the real crate
//!    (`lang-rust/src/sidecar.rs` is the pattern); the template ships none on purpose.
//!
//! What this skeleton already gets right, so you don't have to re-learn it:
//! - the write path is the shared `ci_edit::commit_edits` spine (VFS → gate → commit-or-roll-back;
//!   atomic batches, dry-run, bottom-up same-file ops, transient materialization of created files);
//! - the syntactic import graph is served to the gate **transitively**
//!   (`ci_core::transitive_reverse_imports`) — a syntactic graph does not flatten re-exports,
//!   and a one-hop radius lets a barrel hide its consumers (measured: bench T9-barrel);
//! - the engine starts lazily and [`prewarm`](LanguageProvider::prewarm) warms it off-thread,
//!   so the first `apply_edits` doesn't pay a cold project load inline.

use ci_core::{CommitResult, EditOp, EditOpts, Error, Granularity, ImportGraph, LanguageProvider, Node, Result};
use ci_edit::GateEngine;
use lang_fallback::{FallbackProvider, FbLang};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Builds the language's checker on first use. The real crate hardcodes its own factory
/// (an `LspClient::start` with the checker's command); the template takes it as a parameter
/// so the wiring is testable without any external tool.
pub type EngineFactory = Arc<dyn Fn(&Path) -> Result<Box<dyn GateEngine + Send>> + Send + Sync>;

/// The Step-1 provider: generic tree-sitter reads, a real checker as the gate.
#[derive(Clone)]
pub struct GatedTreeSitter {
    root: PathBuf,
    read: FallbackProvider,
    engine_factory: EngineFactory,
    engine: Arc<Mutex<Option<Box<dyn GateEngine + Send>>>>,
}

impl GatedTreeSitter {
    pub fn new(root: &Path, lang: FbLang, engine_factory: EngineFactory) -> Self {
        Self {
            root: root.to_path_buf(),
            read: FallbackProvider::new(root, lang),
            engine_factory,
            engine: Arc::new(Mutex::new(None)),
        }
    }
}

impl LanguageProvider for GatedTreeSitter {
    fn granularity(&self) -> Granularity {
        Granularity::Ast
    }

    fn structure(&self, file: &Path) -> Result<Vec<Node>> {
        self.read.structure(file)
    }

    fn import_graph(&self) -> Result<ImportGraph> {
        self.read.import_graph()
    }

    // Step-1 providers ARE gated — that's the point. If your checker can be absent at runtime,
    // don't override this to false: fail construction with an install hint instead
    // (contract clause 6: never silently degrade to a weaker gate).
    fn gated(&self) -> bool {
        true
    }

    fn prewarm(&self) {
        let slot = self.engine.clone();
        let factory = self.engine_factory.clone();
        let root = self.root.clone();
        std::thread::spawn(move || {
            let Ok(mut guard) = slot.lock() else { return };
            if guard.is_some() {
                return;
            }
            if let Ok(mut engine) = factory(&root) {
                let _ = engine.diagnostics(&[]);
                *guard = Some(engine);
            }
        });
    }

    fn apply_edits(&self, ops: &[EditOp], opts: &EditOpts) -> Result<CommitResult> {
        let mut guard = self.engine.lock().map_err(|_| Error::Driver("engine lock poisoned".into()))?;
        if guard.is_none() {
            *guard = Some((self.engine_factory)(&self.root)?);
        }
        let engine: &mut dyn GateEngine = guard.as_mut().unwrap().as_mut();
        let structure_of = |f: &str| self.read.structure(Path::new(f)).unwrap_or_default();
        // The blast radius comes from the SYNTACTIC import graph, so it must be served
        // transitively: syntactic edges don't flatten re-exports, and a one-hop radius lets a
        // barrel hide its consumers from the gate (contract clause 3; measured, bench T9).
        let reverse = ci_core::reverse_import_map(&self.read.import_graph().unwrap_or_default());
        let reverse_imports = |file: &str| ci_core::transitive_reverse_imports(&reverse, file);
        ci_edit::commit_edits(&self.root, ops, &structure_of, engine, opts, &reverse_imports)
    }
}

/// Test scaffolding: a deterministic in-process "checker" so the template's wiring (and a
/// copied crate's, before its real engine lands) can run the conformance edit battery without
/// external tools. It flags any file containing [`mock::MARKER`] — your "type error".
pub mod mock {
    use super::*;
    use ci_core::Diag;
    use serde_json::{json, Value};

    /// Content containing this string is diagnosed as an error — a stand-in for a type error.
    pub const MARKER: &str = "TEMPLATE_TYPE_ERROR";

    pub struct MockChecker;

    impl GateEngine for MockChecker {
        fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
            Ok(files
                .iter()
                .filter(|(_, content)| content.contains(MARKER))
                .map(|(file, content)| Diag {
                    file: file.clone(),
                    code: 1,
                    message: format!("mock checker: `{MARKER}` present"),
                    line: content.lines().position(|l| l.contains(MARKER)).unwrap_or(0) as u32 + 1,
                })
                .collect())
        }

        fn rename(&mut self, _file: &str, _line: u32, _character: u32, _new_name: &str) -> Result<Value> {
            Ok(json!({}))
        }

        fn will_rename(&mut self, _from: &str, _to: &str) -> Result<Value> {
            Ok(json!({}))
        }
    }

    /// An [`EngineFactory`] for the mock — what the template's tests inject.
    pub fn factory() -> EngineFactory {
        Arc::new(|_root| Ok(Box::new(MockChecker) as Box<dyn GateEngine + Send>))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(root: &Path) -> GatedTreeSitter {
        GatedTreeSitter::new(root, FbLang::Go, mock::factory())
    }

    // The skeleton's whole promise in one test: reads come from tree-sitter, a gate-flagged
    // edit rejects ATOMICALLY, a clean edit commits — before any real checker exists.
    #[test]
    fn template_gates_and_commits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("svc.go"), "package svc\n\nfunc Probe(url string) bool {\n\treturn true\n}\n").unwrap();
        let p = provider(root);
        let opts = EditOpts { write: true, dry_run: false, tsconfig: None };

        assert!(p.gated(), "Step-1 providers are gated");
        assert!(p.structure(Path::new("svc.go")).unwrap().iter().any(|n| n.id == "svc.go#Probe"));

        let before = std::fs::read_to_string(root.join("svc.go")).unwrap();
        let bad = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "svc.go#Probe".into(),
                    old_text: "return true".into(),
                    new_text: format!("return true // {}", mock::MARKER),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(bad, CommitResult::Rejected { .. }), "checker-flagged edit rejects: {bad:?}");
        assert_eq!(std::fs::read_to_string(root.join("svc.go")).unwrap(), before, "reject leaves disk untouched");

        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "svc.go#Probe".into(),
                    old_text: "return true".into(),
                    new_text: "return false".into(),
                }],
                &opts,
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "clean edit commits: {ok:?}");
        assert!(std::fs::read_to_string(root.join("svc.go")).unwrap().contains("return false"));
    }

    // Pre-existing checker findings are BASELINE (contract clause 5): they never block an
    // unrelated edit — only breakage the batch INTRODUCES rejects.
    #[test]
    fn preexisting_findings_never_block() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("svc.go"),
            format!("package svc\n\n// {}\nfunc Probe(url string) bool {{\n\treturn true\n}}\n", mock::MARKER),
        )
        .unwrap();
        let p = provider(root);
        let ok = p
            .apply_edits(
                &[EditOp::ReplaceText {
                    node_id: "svc.go#Probe".into(),
                    old_text: "return true".into(),
                    new_text: "return false".into(),
                }],
                &EditOpts { write: true, dry_run: false, tsconfig: None },
            )
            .unwrap();
        assert!(matches!(ok, CommitResult::Ok { .. }), "baseline-diff: pre-existing findings are excused: {ok:?}");
    }
}
