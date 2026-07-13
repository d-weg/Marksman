//! The Rust gate: `RustEngine` (rust-analyzer for rename/willRename, `cargo check` for the
//! verdict) plus the deleted-reference gap-fill diagnostics rust-analyzer never reports.
use ci_edit::GateEngine;
use ci_lsp::LspClient;
use ci_core::{Result, Sandbox};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::movefix;

/// The Rust write engine: rust-analyzer for diagnostics/rename, plus a SYNTACTIC module-move
/// fallback for the one operation ra's `willRenameFiles` doesn't cover (moves into a
/// submodule return NO edits, leaving the `mod` decl and every `crate::` path dangling —
/// bench `move-rust`). The fallback emits a genuine WorkspaceEdit (see `movefix`); the gate
/// still verifies the result, so an unsupported shape degrades to a REJECT with named sites,
/// never a silent break.
pub(crate) struct RustEngine {
    pub(crate) root: PathBuf,
    pub(crate) lsp: LspClient,
    /// Where the toolchain runs (`ci_core::resolve_sandbox`). `HostSandbox` today; the one seam a
    /// container backend swaps in — see `docs/container-gate-spec.md`.
    pub(crate) sandbox: Arc<dyn Sandbox>,
}

/// Diagnostics for references to files the CURRENT BATCH deletes (empty-content buffers, the
/// gate's deletion convention): `use crate::a::b…` chains and `mod x;` decls resolving to a
/// deleted path. This is the E0432/E0583 class rust-analyzer's pull diagnostics never report.
/// The scan is the shared §8 engine over the Rust hooks — the same occurrences the move
/// rewriter consumes.
fn deleted_path_references(root: &Path, files: &[(String, String)]) -> Vec<ci_core::Diag> {
    ci_edit::moves::deleted_reference_diags(&movefix::RustMoveModel(root), files)
}

/// rustc-grade gate: transiently materialize the candidate buffers, run
/// `cargo check --message-format=json`, map primary-span errors to Diags, restore the disk.
/// Whole-crate by nature — errors OUTSIDE the computed radius are reported too (sounder than
/// the radius; the baseline diff still excuses pre-existing ones). Buffer conventions from
/// ci-edit hold: an EMPTY buffer for a path already off disk is a staged deletion's stand-in
/// and must NOT be recreated (that would resurrect the module for the check).
fn cargo_check_diags(root: &Path, sandbox: &dyn Sandbox, files: &[(String, String)]) -> Result<Vec<ci_core::Diag>> {
    struct Restore(Vec<(std::path::PathBuf, Option<String>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (p, orig) in &self.0 {
                match orig {
                    Some(c) => {
                        let _ = std::fs::write(p, c);
                    }
                    None => {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }
        }
    }
    let t0 = std::time::Instant::now();
    let mut guard = Restore(Vec::new());
    for (rel, content) in files {
        let abs = root.join(rel);
        let on_disk = std::fs::read_to_string(&abs).ok();
        if content.is_empty() && on_disk.is_none() {
            continue; // deletion stand-in: the file is (correctly) gone from disk
        }
        if on_disk.as_deref() == Some(content.as_str()) {
            continue; // buffer already equals disk (baseline pass): nothing to stage
        }
        if let Some(parent) = abs.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&abs, content)
            .map_err(|e| ci_core::Error::Driver(format!("stage {rel} for cargo check: {e}")))?;
        guard.0.push((abs, on_disk));
    }
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(["check", "--message-format=json", "-q"])
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never");
    // Capped + time-bounded (run_gate_capped): a hung `cargo check` can't hang the edit — and,
    // since the MCP server loop is single-threaded, the whole server — forever; a timeout REFUSES
    // the edit (Error::GateTimeout propagates; see `diagnostics` below). Truncation at the cap is
    // sound: a dropped diagnostic can only under-report on an already-failing exit code, and the
    // silent-tool-failure invariant below still rejects that.
    let out = ci_core::run_gate_capped(sandbox, &mut cmd, "cargo check")?;
    let mut diags = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["reason"] != "compiler-message" || v["message"]["level"] != "error" {
            continue;
        }
        let msg = &v["message"];
        // Spanless errors ("aborting due to N previous errors") carry no site — skip.
        let Some(span) = msg["spans"].as_array().and_then(|s| s.iter().find(|sp| sp["is_primary"] == true)) else {
            continue;
        };
        let file = span["file_name"].as_str().unwrap_or("").replace('\\', "/");
        if file.is_empty() {
            continue;
        }
        let text = msg["message"].as_str().unwrap_or("").to_string();
        // rustc codes are strings ("E0308"); Diag.code is the numeric TS convention — carry
        // the rustc code in the MESSAGE (code stays 0 so no "TS…" prefix is rendered).
        let message = match msg["code"]["code"].as_str() {
            Some(c) if !text.starts_with(c) => format!("{c}: {text}"),
            _ => text,
        };
        diags.push(ci_core::Diag {
            file,
            code: 0,
            message,
            line: span["line_start"].as_u64().unwrap_or(0) as u32,
        });
    }
    // Reject-on-failed-tool: errors but none parsed (a broken Cargo.toml aborts before
    // compiler-messages) surfaces as a diagnostic so the gate REJECTS, never reads silence as clean.
    if let Some(d) = ci_core::silent_tool_failure_diag(
        out.status.is_some_and(|s| s.success()),
        &diags,
        "Cargo.toml",
        || {
            let err = String::from_utf8_lossy(&out.stderr);
            err.lines().find(|l| !l.trim().is_empty()).unwrap_or("cargo check failed").to_string()
        },
    ) {
        diags.push(d);
    }
    if std::env::var("CI_TIMING").is_ok() {
        eprintln!("[timing]   cargo check gate {:?} ({} staged, {} errors)", t0.elapsed(), guard.0.len(), diags.len());
    }
    Ok(diags)
}

impl GateEngine for RustEngine {
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<ci_core::Diag>> {
        // The gate VERDICT comes from rustc (`cargo check`), not rust-analyzer: ra's native
        // pull diagnostics have two verified coverage holes — unresolved imports (bench
        // move-rust round 4, gap-filled syntactically) and trait/operator errors (bench
        // locate-edit: `const RRF_K: f64` committed "type-checked clean" while `RRF_K +
        // rank as f32` was E0277 — a class no syntactic gap-fill can catch). Measured on the
        // fixture: warm incremental `cargo check` is 0.1–0.25s, CHEAPER than ra's quiescence
        // dance. Candidate buffers are materialized transiently (drop-guard restores), so
        // rustc sees exactly the state the batch proposes. ra remains the rename/willRename
        // engine; CI_RUST_GATE=ra restores the old path (plus gap-fill) as an escape hatch.
        if std::env::var("CI_RUST_GATE").as_deref() == Ok("ra") {
            let mut out = self.lsp.diagnostics(files)?;
            out.extend(deleted_path_references(&self.root, files));
            return Ok(out);
        }
        match cargo_check_diags(&self.root, &*self.sandbox, files) {
            Ok(diags) => Ok(diags),
            // A TIMEOUT is not "cargo unavailable" — the tool exists and hung. It must REFUSE
            // the edit (disk already restored by the drop-guard), never swap in rust-analyzer:
            // silently trading the rustc verdict for ra's weaker one on a hang is exactly the
            // gate degrade CONTRIBUTING forbids. (Emitting the timeout as a Diag would be just
            // as wrong: baseline and after passes would both carry it and the baseline diff
            // would excuse it — a false pass.)
            Err(e @ ci_core::Error::GateTimeout(_)) => Err(e),
            Err(e) => {
                // cargo unavailable (unusual: ra requires a toolchain) — degrade to ra +
                // gap-fill rather than blocking every edit, but say so.
                eprintln!("[lang-rust] cargo check gate unavailable ({e}); falling back to rust-analyzer diagnostics");
                let mut out = self.lsp.diagnostics(files)?;
                out.extend(deleted_path_references(&self.root, files));
                Ok(out)
            }
        }
    }
    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<serde_json::Value> {
        GateEngine::rename(&mut self.lsp, file, line, character, new_name)
    }
    fn will_rename(&mut self, from: &str, to: &str) -> Result<serde_json::Value> {
        // movefix FIRST: for the move shapes it understands, ra's willRenameFiles both
        // returns NOTHING and takes ~12s to say so (the request queues behind cache priming
        // on the main loop — measured: 12.4s of an 18s gate). movefix is deterministic
        // syntax, and the type-check gate rejects any rewrite it gets wrong, so asking ra
        // first buys nothing but the wait. Shapes movefix declines still go to ra.
        if let Some(fix) = movefix::move_workspace_edit(&self.root, from, to) {
            return Ok(fix);
        }
        GateEngine::will_rename(&mut self.lsp, from, to)
    }
    fn sync_disk(&mut self) -> Result<()> {
        self.lsp.sync_disk()
    }
    fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        self.lsp.fs_events(created, deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The no-degrade pin: a hung `cargo check` (run_capped reports timed_out) must surface as
    // Error::GateTimeout — the distinguished variant `RustEngine::diagnostics` PROPAGATES instead
    // of taking the rust-analyzer fallback. If this ever becomes a plain Driver error, a hang
    // silently swaps the verdict engine.
    #[test]
    fn cargo_check_timeout_is_gate_timeout_not_driver() {
        struct HungSandbox;
        impl Sandbox for HungSandbox {
            fn run_capped(
                &self,
                _cmd: &mut std::process::Command,
                _timeout: std::time::Duration,
                _cap: usize,
            ) -> std::io::Result<ci_core::CappedOutput> {
                Ok(ci_core::CappedOutput {
                    status: None,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    timed_out: true,
                })
            }
            fn spawn(&self, _cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
                unreachable!("the verdict path never spawns a resident process")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let files = vec![("src/lib.rs".to_string(), "pub fn x() {}\n".to_string())];
        let err = cargo_check_diags(dir.path(), &HungSandbox, &files).unwrap_err();
        assert!(
            matches!(err, ci_core::Error::GateTimeout(_)),
            "a timed-out gate is GateTimeout (refuse), never Driver (which would take the ra fallback): {err:?}"
        );
        // The drop-guard restored the transient staging: nothing left on disk.
        assert!(!dir.path().join("src/lib.rs").exists(), "staged buffer restored on the error path");
    }

    // The extraction insurance: the rustc-shaped E0432/E0583 messages (and their sites) are
    // reply surface the delete-refusal / move flows depend on, so the generic §8 form over
    // the Rust hooks must keep them byte-for-byte.
    #[test]
    fn deleted_path_references_pin_the_rustc_shaped_messages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod gone;\npub mod user;\n").unwrap();
        std::fs::write(root.join("src/gone.rs"), "pub fn g() -> i32 { 1 }\n").unwrap();
        std::fs::write(root.join("src/user.rs"), "pub use crate::gone::g;\n").unwrap();

        let files = vec![
            ("src/lib.rs".to_string(), "pub mod gone;\npub mod user;\n".to_string()),
            ("src/user.rs".to_string(), "pub use crate::gone::g;\n".to_string()),
            ("src/gone.rs".to_string(), String::new()), // the batch's deletion stand-in
        ];
        let diags = deleted_path_references(root, &files);
        let msgs: Vec<(&str, u32, &str)> =
            diags.iter().map(|d| (d.file.as_str(), d.line, d.message.as_str())).collect();
        assert_eq!(
            msgs,
            vec![
                (
                    "src/lib.rs",
                    1,
                    "`mod gone` points at src/gone.rs, which this batch deletes/moves (E0583); update or remove the declaration"
                ),
                (
                    "src/user.rs",
                    1,
                    "unresolved import `crate::gone` — src/gone.rs is deleted/moved by this batch (E0432); update the path"
                ),
            ],
            "E0583 decl-side + E0432 use-side, exact sites and wording: {diags:?}"
        );

        // No deletions in the batch → nothing to report.
        let clean = vec![("src/user.rs".to_string(), "pub use crate::gone::g;\n".to_string())];
        assert!(deleted_path_references(root, &clean).is_empty());
    }
}
